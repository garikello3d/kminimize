use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse Kconfig at {path}: {reason}")]
    ParseError { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Expression over CONFIG symbols, as defined by the Kconfig language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigExpr {
    Symbol(String),
    Literal(String),
    /// Unconditionally true (missing `depends on`, `obj-y`).
    Always,
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Not(Box<Self>),
    Equal(Box<Self>, Box<Self>),
    NotEqual(Box<Self>, Box<Self>),
    Less(Box<Self>, Box<Self>),
    LessEq(Box<Self>, Box<Self>),
    Greater(Box<Self>, Box<Self>),
    GreaterEq(Box<Self>, Box<Self>),
}

impl ConfigExpr {
    pub fn symbols(&self) -> HashSet<&str> {
        let mut out = HashSet::new();
        self.collect_symbols(&mut out);
        out
    }

    fn collect_symbols<'a>(&'a self, out: &mut HashSet<&'a str>) {
        match self {
            Self::Symbol(s) => { out.insert(s.as_str()); }
            Self::Literal(_) | Self::Always => {}
            Self::Not(a) => a.collect_symbols(out),
            Self::And(a, b) | Self::Or(a, b)
            | Self::Equal(a, b) | Self::NotEqual(a, b)
            | Self::Less(a, b) | Self::LessEq(a, b)
            | Self::Greater(a, b) | Self::GreaterEq(a, b) => {
                a.collect_symbols(out);
                b.collect_symbols(out);
            }
        }
    }

    pub fn eval_bool(&self, values: &HashMap<&str, &str>) -> bool {
        match self {
            Self::Always => true,
            Self::Symbol(s) => {
                let v = values.get(s.as_str()).copied().unwrap_or("n");
                v == "y" || v == "m"
            }
            Self::Literal(s) => s == "y" || s == "m",
            Self::Not(a) => !a.eval_bool(values),
            Self::And(a, b) => a.eval_bool(values) && b.eval_bool(values),
            Self::Or(a, b) => a.eval_bool(values) || b.eval_bool(values),
            Self::Equal(a, b) => compare_values(a, b, values) == std::cmp::Ordering::Equal,
            Self::NotEqual(a, b) => compare_values(a, b, values) != std::cmp::Ordering::Equal,
            Self::Less(a, b) => compare_values(a, b, values) == std::cmp::Ordering::Less,
            Self::LessEq(a, b) => compare_values(a, b, values) != std::cmp::Ordering::Greater,
            Self::Greater(a, b) => compare_values(a, b, values) == std::cmp::Ordering::Greater,
            Self::GreaterEq(a, b) => compare_values(a, b, values) != std::cmp::Ordering::Less,
        }
    }
}

fn resolve_str<'a>(expr: &'a ConfigExpr, values: &HashMap<&str, &'a str>) -> &'a str {
    match expr {
        ConfigExpr::Symbol(s) => values.get(s.as_str()).copied().unwrap_or("n"),
        ConfigExpr::Literal(s) => s.as_str(),
        _ => "n",
    }
}

fn compare_values(a: &ConfigExpr, b: &ConfigExpr, values: &HashMap<&str, &str>) -> std::cmp::Ordering {
    let av = resolve_str(a, values);
    let bv = resolve_str(b, values);
    // Numeric if both parse as integer (decimal or 0x hex)
    if let (Some(an), Some(bn)) = (parse_int(av), parse_int(bv)) {
        an.cmp(&bn)
    } else {
        av.cmp(bv)
    }
}

fn parse_int(s: &str) -> Option<i64> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<i64>().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolKind {
    Bool,
    Tristate,
    Int,
    Hex,
    String,
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub depends_on: Option<ConfigExpr>,
    /// `select TARGET [if COND]`
    pub selects: Vec<(String, ConfigExpr)>,
    pub implies: Vec<(String, ConfigExpr)>,
    pub choice_group: Option<String>,
}

pub struct KconfigGraph {
    symbols: HashMap<String, Symbol>,
}

impl KconfigGraph {
    pub fn for_test(symbols: HashMap<String, Symbol>) -> Self {
        Self { symbols }
    }

    pub fn load(kernel_src: &Path) -> Result<Self> {
        let root = kernel_src.join("Kconfig");
        let mut symbols: HashMap<String, Symbol> = HashMap::new();
        let mut visited: HashSet<PathBuf> = HashSet::new();
        parse_kconfig_file(&root, kernel_src, &[], &mut symbols, &mut visited)?;
        Ok(Self { symbols })
    }

    pub fn symbol(&self, name: &str) -> Option<&Symbol> {
        self.symbols.get(name)
    }

    /// Returns all symbols that must also be disabled when `config` is disabled,
    /// because they `select` it — either unconditionally or via a condition that
    /// is currently active in `values`.
    ///
    /// BFS follows an edge `selector →[cond]→ target` when:
    ///   - `cond == Always`, OR
    ///   - `eval_bool(cond, values) == true` AND the selector itself is enabled.
    ///
    /// `values` should be the current (pre-change) config map.  Using pre-change
    /// values is conservative: it may include selectors whose condition would
    /// become false once the target is disabled, but that is always safe — it
    /// suggests disabling more symbols, never fewer.
    pub fn disable_cascade(&self, config: &str, values: &HashMap<&str, &str>) -> HashSet<String> {
        // Build reverse-select map (all edges, not just unconditional):
        // target → Vec<(selector, condition)>
        let mut rev: HashMap<&str, Vec<(&str, &ConfigExpr)>> = HashMap::new();
        for sym in self.symbols.values() {
            for (target, cond) in &sym.selects {
                rev.entry(target.as_str())
                    .or_default()
                    .push((sym.name.as_str(), cond));
            }
        }

        fn is_enabled(name: &str, values: &HashMap<&str, &str>) -> bool {
            matches!(values.get(name).copied(), Some("y") | Some("m"))
        }

        let mut result: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        queue.push_back(config);
        while let Some(cur) = queue.pop_front() {
            if let Some(edges) = rev.get(cur) {
                for &(sel, cond) in edges {
                    let active = *cond == ConfigExpr::Always
                        || (cond.eval_bool(values) && is_enabled(sel, values));
                    if active && sel != config && result.insert(sel.to_owned()) {
                        queue.push_back(sel);
                    }
                }
            }
        }
        result
    }

    /// Splits `cascade` (the output of `disable_cascade`) into two sets:
    /// - unconditional selectors (their `select` has no condition)
    /// - conditionally-active selectors (their `select if COND` is currently true)
    ///
    /// A selector can appear in both sets if it has multiple select statements
    /// for the same target with different conditions.
    pub fn classify_cascade(
        &self,
        target: &str,
        cascade: &HashSet<String>,
        values: &HashMap<&str, &str>,
    ) -> (HashSet<String>, HashSet<String>) {
        let mut unconditional: HashSet<String> = HashSet::new();
        let mut active: HashSet<String> = HashSet::new();
        for sel_name in cascade {
            if let Some(sym) = self.symbols.get(sel_name) {
                for (t, cond) in &sym.selects {
                    if t != target {
                        continue;
                    }
                    if *cond == ConfigExpr::Always {
                        unconditional.insert(sel_name.clone());
                    } else if cond.eval_bool(values) {
                        active.insert(sel_name.clone());
                    }
                }
            }
        }
        (unconditional, active)
    }

    /// Returns all CONFIG symbols that transitively become disabled because their
    /// `depends on` expression becomes false once `initially_disabled` symbols are
    /// set to `n`.  Only considers symbols currently enabled (y/m) in `values`.
    /// Does NOT include `initially_disabled` itself in the return set.
    pub fn transitive_depends_disabled(
        &self,
        initially_disabled: &HashSet<String>,
        values: &HashMap<&str, &str>,
    ) -> HashSet<String> {
        let mut all_disabled: HashSet<String> = initially_disabled.clone();

        loop {
            let mut merged: HashMap<&str, &str> = values.clone();
            for s in &all_disabled {
                merged.insert(s.as_str(), "n");
            }

            let mut newly: Vec<String> = Vec::new();
            for (name, sym) in &self.symbols {
                if all_disabled.contains(name) {
                    continue;
                }
                if !matches!(values.get(name.as_str()).copied(), Some("y") | Some("m")) {
                    continue;
                }
                if let Some(dep) = &sym.depends_on {
                    if dep.eval_bool(values) && !dep.eval_bool(&merged) {
                        newly.push(name.clone());
                    }
                }
            }

            if newly.is_empty() {
                break;
            }
            all_disabled.extend(newly);
        }

        all_disabled.difference(initially_disabled).cloned().collect()
    }

    /// Returns the subset of `disabled` that would be re-enabled by an active
    /// `select` from a symbol not in `disabled`.
    ///
    /// An edge is a conflict when the selector is currently enabled (per
    /// `values`) and its select condition is satisfied.  If `disable_cascade`
    /// was called with the same `values`, a non-empty result indicates that the
    /// cascade was incomplete (a rare edge case involving complex conditions).
    pub fn validate_disabled(
        &self,
        disabled: &HashSet<String>,
        values: &HashMap<&str, &str>,
    ) -> HashSet<String> {
        fn is_enabled(name: &str, values: &HashMap<&str, &str>) -> bool {
            matches!(values.get(name).copied(), Some("y") | Some("m"))
        }
        let mut conflicts: HashSet<String> = HashSet::new();
        for sym in self.symbols.values() {
            if disabled.contains(&sym.name) {
                continue;
            }
            if !is_enabled(&sym.name, values) {
                continue; // disabled selector can't force anything on
            }
            for (target, cond) in &sym.selects {
                if disabled.contains(target) {
                    let active = *cond == ConfigExpr::Always || cond.eval_bool(values);
                    if active {
                        conflicts.insert(target.clone());
                    }
                }
            }
        }
        conflicts
    }
}

// ── Kconfig parser ────────────────────────────────────────────────────────────

fn parse_kconfig_file(
    path: &Path,
    kernel_src: &Path,
    parent_conditions: &[ConfigExpr],
    symbols: &mut HashMap<String, Symbol>,
    visited: &mut HashSet<PathBuf>,
) -> Result<()> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canon.clone()) {
        return Ok(());
    }
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    let lines = join_continuations(&text);
    let mut i = 0;
    // Stack of `if` block conditions
    let mut if_stack: Vec<ConfigExpr> = parent_conditions.to_vec();
    // Current symbol being built
    let mut cur_sym: Option<Symbol> = None;
    // Current choice group name (if inside a choice block)
    let mut choice_group: Option<String> = None;
    // choice nesting depth for tracking end
    let mut choice_depth: usize = 0;

    while i < lines.len() {
        let line = lines[i].trim().to_string();
        i += 1;

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Flush previous symbol when we hit a new block keyword
        let first_word = line.split_whitespace().next().unwrap_or("");

        match first_word {
            "config" | "menuconfig" => {
                if let Some(sym) = cur_sym.take() {
                    symbols.insert(sym.name.clone(), sym);
                }
                let name = line.split_whitespace().nth(1).unwrap_or("").to_uppercase();
                // Remove CONFIG_ prefix if present (shouldn't be in Kconfig files, but be safe)
                let name = name.strip_prefix("CONFIG_").unwrap_or(&name).to_owned();
                cur_sym = Some(Symbol {
                    name,
                    kind: SymbolKind::Bool,
                    depends_on: effective_condition(&if_stack),
                    selects: Vec::new(),
                    implies: Vec::new(),
                    choice_group: choice_group.clone(),
                });
            }
            "bool" | "tristate" | "int" | "hex" | "string" => {
                if let Some(sym) = cur_sym.as_mut() {
                    sym.kind = match first_word {
                        "bool" => SymbolKind::Bool,
                        "tristate" => SymbolKind::Tristate,
                        "int" => SymbolKind::Int,
                        "hex" => SymbolKind::Hex,
                        _ => SymbolKind::String,
                    };
                }
            }
            "depends" => {
                // `depends on EXPR`
                if let Some(sym) = cur_sym.as_mut() {
                    let rest = line
                        .strip_prefix("depends on ")
                        .or_else(|| line.strip_prefix("depends\ton "))
                        .unwrap_or("")
                        .trim();
                    if !rest.is_empty() {
                        let extra = parse_expr(rest);
                        sym.depends_on = Some(match sym.depends_on.take() {
                            None => {
                                and_with_stack(extra, &if_stack)
                            }
                            Some(existing) => ConfigExpr::And(
                                Box::new(existing),
                                Box::new(and_with_stack(extra, &if_stack)),
                            ),
                        });
                    }
                }
            }
            "select" => {
                if let Some(sym) = cur_sym.as_mut() {
                    parse_select_line(&line, &mut sym.selects);
                }
            }
            "imply" => {
                if let Some(sym) = cur_sym.as_mut() {
                    parse_select_line(&line, &mut sym.implies);
                }
            }
            "if" => {
                if let Some(sym) = cur_sym.take() {
                    symbols.insert(sym.name.clone(), sym);
                }
                let rest = line.strip_prefix("if").unwrap_or("").trim().to_string();
                if !rest.is_empty() {
                    if_stack.push(parse_expr(&rest));
                }
            }
            "endif" => {
                if let Some(sym) = cur_sym.take() {
                    symbols.insert(sym.name.clone(), sym);
                }
                if !if_stack.is_empty() {
                    if_stack.pop();
                }
            }
            "choice" => {
                if let Some(sym) = cur_sym.take() {
                    symbols.insert(sym.name.clone(), sym);
                }
                // Extract prompt if any: `choice\nprompt "name"` — look ahead
                let group = if i < lines.len() && lines[i].trim().starts_with("prompt") {
                    let p = lines[i].trim().to_string();
                    i += 1;
                    p.strip_prefix("prompt").unwrap_or("").trim().trim_matches('"').to_owned()
                } else {
                    format!("choice_{}", symbols.len())
                };
                choice_group = Some(group);
                choice_depth += 1;
            }
            "endchoice" => {
                if let Some(sym) = cur_sym.take() {
                    symbols.insert(sym.name.clone(), sym);
                }
                choice_depth = choice_depth.saturating_sub(1);
                if choice_depth == 0 {
                    choice_group = None;
                }
            }
            "menu" | "mainmenu" | "comment" => {
                if let Some(sym) = cur_sym.take() {
                    symbols.insert(sym.name.clone(), sym);
                }
            }
            "endmenu" => {
                if let Some(sym) = cur_sym.take() {
                    symbols.insert(sym.name.clone(), sym);
                }
            }
            "source" | "rsource" | "osource" => {
                if let Some(sym) = cur_sym.take() {
                    symbols.insert(sym.name.clone(), sym);
                }
                let raw = line
                    .strip_prefix(first_word)
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"');
                // rsource is relative to current file; source is relative to kernel_src
                let included = if first_word == "rsource" {
                    path.parent().unwrap_or(kernel_src).join(raw)
                } else {
                    kernel_src.join(raw)
                };
                if included.exists() {
                    parse_kconfig_file(&included, kernel_src, &if_stack, symbols, visited)?;
                }
            }
            _ => {}
        }
    }
    if let Some(sym) = cur_sym.take() {
        symbols.insert(sym.name.clone(), sym);
    }
    Ok(())
}

fn join_continuations(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.ends_with('\\') {
            current.push_str(&line[..line.len() - 1]);
            current.push(' ');
        } else {
            current.push_str(line);
            result.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

fn effective_condition(stack: &[ConfigExpr]) -> Option<ConfigExpr> {
    stack.iter().cloned().reduce(|a, b| ConfigExpr::And(Box::new(a), Box::new(b)))
}

fn and_with_stack(expr: ConfigExpr, stack: &[ConfigExpr]) -> ConfigExpr {
    match effective_condition(stack) {
        None => expr,
        Some(cond) => ConfigExpr::And(Box::new(cond), Box::new(expr)),
    }
}

/// Parse `select TARGET [if EXPR]` or `imply TARGET [if EXPR]`
fn parse_select_line(line: &str, out: &mut Vec<(String, ConfigExpr)>) {
    // strip leading keyword
    let rest = line.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim();
    if rest.is_empty() {
        return;
    }
    if let Some(idx) = find_if_keyword(rest) {
        let target = rest[..idx].trim().to_uppercase();
        let cond_str = rest[idx + 3..].trim(); // skip " if"
        out.push((target, parse_expr(cond_str)));
    } else {
        out.push((rest.to_uppercase(), ConfigExpr::Always));
    }
}

/// Find the index of a bare ` if ` separator in a select line (not inside parens).
fn find_if_keyword(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b' ' | b'\t' if depth == 0 => {
                if s[i..].starts_with(" if ") || s[i..].starts_with("\tif ") {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ── Expression parser ─────────────────────────────────────────────────────────

pub fn parse_expr(input: &str) -> ConfigExpr {
    let tokens = tokenize_expr(input);
    let mut pos = 0;
    let expr = parse_or(&tokens, &mut pos);
    expr
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Sym(String),
    Lit(String),
    And,
    Or,
    Not,
    Eq,
    NEq,
    Lt,
    Le,
    Gt,
    Ge,
    LParen,
    RParen,
}

fn tokenize_expr(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            ' ' | '\t' => { i += 1; }
            '(' => { tokens.push(Token::LParen); i += 1; }
            ')' => { tokens.push(Token::RParen); i += 1; }
            '!' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::NEq); i += 2;
                } else {
                    tokens.push(Token::Not); i += 1;
                }
            }
            '=' => { tokens.push(Token::Eq); i += 1; }
            '<' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::Le); i += 2;
                } else {
                    tokens.push(Token::Lt); i += 1;
                }
            }
            '>' => {
                if i + 1 < chars.len() && chars[i + 1] == '=' {
                    tokens.push(Token::Ge); i += 2;
                } else {
                    tokens.push(Token::Gt); i += 1;
                }
            }
            '"' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '"' { i += 1; }
                let s: String = chars[start..i].iter().collect();
                tokens.push(Token::Lit(s));
                if i < chars.len() { i += 1; } // closing "
            }
            _ => {
                let start = i;
                while i < chars.len() && !" \t()!=<>\"".contains(chars[i]) { i += 1; }
                let word: String = chars[start..i].iter().collect();
                match word.as_str() {
                    "&&" | "-a" => tokens.push(Token::And),
                    "||" | "-o" => tokens.push(Token::Or),
                    "!" => tokens.push(Token::Not),
                    "y" | "m" | "n" | "" => tokens.push(Token::Lit(word)),
                    w if w.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) => {
                        tokens.push(Token::Lit(word));
                    }
                    _ => tokens.push(Token::Sym(word.to_uppercase())),
                }
            }
        }
    }
    tokens
}

fn parse_or(tokens: &[Token], pos: &mut usize) -> ConfigExpr {
    let mut left = parse_and(tokens, pos);
    while *pos < tokens.len() && tokens[*pos] == Token::Or {
        *pos += 1;
        let right = parse_and(tokens, pos);
        left = ConfigExpr::Or(Box::new(left), Box::new(right));
    }
    left
}

fn parse_and(tokens: &[Token], pos: &mut usize) -> ConfigExpr {
    let mut left = parse_cmp(tokens, pos);
    while *pos < tokens.len() && tokens[*pos] == Token::And {
        *pos += 1;
        let right = parse_cmp(tokens, pos);
        left = ConfigExpr::And(Box::new(left), Box::new(right));
    }
    left
}

fn parse_cmp(tokens: &[Token], pos: &mut usize) -> ConfigExpr {
    let left = parse_unary(tokens, pos);
    if *pos >= tokens.len() {
        return left;
    }
    match &tokens[*pos] {
        Token::Eq => { *pos += 1; let r = parse_unary(tokens, pos); ConfigExpr::Equal(Box::new(left), Box::new(r)) }
        Token::NEq => { *pos += 1; let r = parse_unary(tokens, pos); ConfigExpr::NotEqual(Box::new(left), Box::new(r)) }
        Token::Lt => { *pos += 1; let r = parse_unary(tokens, pos); ConfigExpr::Less(Box::new(left), Box::new(r)) }
        Token::Le => { *pos += 1; let r = parse_unary(tokens, pos); ConfigExpr::LessEq(Box::new(left), Box::new(r)) }
        Token::Gt => { *pos += 1; let r = parse_unary(tokens, pos); ConfigExpr::Greater(Box::new(left), Box::new(r)) }
        Token::Ge => { *pos += 1; let r = parse_unary(tokens, pos); ConfigExpr::GreaterEq(Box::new(left), Box::new(r)) }
        _ => left,
    }
}

fn parse_unary(tokens: &[Token], pos: &mut usize) -> ConfigExpr {
    if *pos < tokens.len() && tokens[*pos] == Token::Not {
        *pos += 1;
        let inner = parse_unary(tokens, pos);
        return ConfigExpr::Not(Box::new(inner));
    }
    parse_primary(tokens, pos)
}

fn parse_primary(tokens: &[Token], pos: &mut usize) -> ConfigExpr {
    if *pos >= tokens.len() {
        return ConfigExpr::Always;
    }
    match &tokens[*pos] {
        Token::LParen => {
            *pos += 1;
            let expr = parse_or(tokens, pos);
            if *pos < tokens.len() && tokens[*pos] == Token::RParen {
                *pos += 1;
            }
            expr
        }
        Token::Sym(s) => {
            let s = s.clone();
            *pos += 1;
            ConfigExpr::Symbol(s)
        }
        Token::Lit(s) => {
            let s = s.clone();
            *pos += 1;
            ConfigExpr::Literal(s)
        }
        _ => ConfigExpr::Always,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals<'a>(pairs: &[(&'a str, &'a str)]) -> HashMap<&'a str, &'a str> {
        pairs.iter().copied().collect()
    }

    fn graph_from(symbols: Vec<Symbol>) -> KconfigGraph {
        let map = symbols.into_iter().map(|s| (s.name.clone(), s)).collect();
        KconfigGraph { symbols: map }
    }

    fn sym(name: &str, selects: Vec<(&str, ConfigExpr)>) -> Symbol {
        Symbol {
            name: name.to_owned(),
            kind: SymbolKind::Bool,
            depends_on: None,
            selects: selects.into_iter().map(|(t, c)| (t.to_owned(), c)).collect(),
            implies: vec![],
            choice_group: None,
        }
    }

    #[test]
    fn eval_bool_symbol() {
        let v = vals(&[("FOO", "y"), ("BAR", "n")]);
        assert!(ConfigExpr::Symbol("FOO".into()).eval_bool(&v));
        assert!(!ConfigExpr::Symbol("BAR".into()).eval_bool(&v));
        assert!(!ConfigExpr::Symbol("MISSING".into()).eval_bool(&v));
    }

    #[test]
    fn eval_bool_and() {
        let v = vals(&[("A", "y"), ("B", "m"), ("C", "n")]);
        let e = ConfigExpr::And(
            Box::new(ConfigExpr::Symbol("A".into())),
            Box::new(ConfigExpr::Symbol("B".into())),
        );
        assert!(e.eval_bool(&v));
        let e2 = ConfigExpr::And(
            Box::new(ConfigExpr::Symbol("A".into())),
            Box::new(ConfigExpr::Symbol("C".into())),
        );
        assert!(!e2.eval_bool(&v));
    }

    #[test]
    fn eval_bool_always() {
        assert!(ConfigExpr::Always.eval_bool(&HashMap::new()));
    }

    #[test]
    fn eval_bool_not() {
        let v = vals(&[("X", "y")]);
        assert!(!ConfigExpr::Not(Box::new(ConfigExpr::Symbol("X".into()))).eval_bool(&v));
        assert!(ConfigExpr::Not(Box::new(ConfigExpr::Symbol("Y".into()))).eval_bool(&v));
    }

    #[test]
    fn eval_bool_comparison_numeric() {
        let v = vals(&[("VER", "5")]);
        let e = ConfigExpr::GreaterEq(
            Box::new(ConfigExpr::Symbol("VER".into())),
            Box::new(ConfigExpr::Literal("4".into())),
        );
        assert!(e.eval_bool(&v));
    }

    #[test]
    fn parse_expr_basic() {
        let e = parse_expr("FOO && BAR");
        assert_eq!(
            e,
            ConfigExpr::And(
                Box::new(ConfigExpr::Symbol("FOO".into())),
                Box::new(ConfigExpr::Symbol("BAR".into())),
            )
        );
    }

    #[test]
    fn disable_cascade_unconditional_only() {
        // FOO unconditionally selects TARGET; BAR selects TARGET with inactive condition
        let g = graph_from(vec![
            sym("FOO", vec![("TARGET", ConfigExpr::Always)]),
            sym("BAR", vec![("TARGET", ConfigExpr::Symbol("COND".into()))]),
            sym("TARGET", vec![]),
        ]);
        // COND is not in values → BAR's condition is inactive
        let cascade = g.disable_cascade("TARGET", &HashMap::new());
        assert!(cascade.contains("FOO"), "FOO must be in cascade (unconditional)");
        assert!(!cascade.contains("BAR"), "BAR's condition is inactive, must not cascade");
    }

    #[test]
    fn disable_cascade_conditional_active() {
        // BAR selects TARGET if COND; COND is currently y and BAR is enabled
        let g = graph_from(vec![
            sym("BAR", vec![("TARGET", ConfigExpr::Symbol("COND".into()))]),
            sym("TARGET", vec![]),
        ]);
        let v = vals(&[("COND", "y"), ("BAR", "y")]);
        let cascade = g.disable_cascade("TARGET", &v);
        assert!(cascade.contains("BAR"), "BAR must appear: conditional select is active");
    }

    #[test]
    fn disable_cascade_conditional_inactive() {
        // Same setup but COND is n → BAR's select is dormant
        let g = graph_from(vec![
            sym("BAR", vec![("TARGET", ConfigExpr::Symbol("COND".into()))]),
            sym("TARGET", vec![]),
        ]);
        let v = vals(&[("COND", "n"), ("BAR", "y")]);
        let cascade = g.disable_cascade("TARGET", &v);
        assert!(!cascade.contains("BAR"), "BAR must NOT appear: conditional select is inactive");
    }

    #[test]
    fn classify_cascade_splits_correctly() {
        let g = graph_from(vec![
            sym("UNCOND", vec![("TARGET", ConfigExpr::Always)]),
            sym("ACTIVE", vec![("TARGET", ConfigExpr::Symbol("COND".into()))]),
            sym("TARGET", vec![]),
        ]);
        let v = vals(&[("COND", "y"), ("UNCOND", "y"), ("ACTIVE", "y")]);
        let cascade = g.disable_cascade("TARGET", &v);
        let (uncond, active) = g.classify_cascade("TARGET", &cascade, &v);
        assert!(uncond.contains("UNCOND"));
        assert!(!uncond.contains("ACTIVE"));
        assert!(active.contains("ACTIVE"));
        assert!(!active.contains("UNCOND"));
    }

    #[test]
    fn validate_disabled_detects_unconditional_conflict() {
        let g = graph_from(vec![
            sym("ENABLED_SYM", vec![("FOO", ConfigExpr::Always)]),
            sym("FOO", vec![]),
        ]);
        let disabled: HashSet<String> = ["FOO".to_owned()].into();
        // ENABLED_SYM is enabled (y) and not in disabled → conflict
        let v = vals(&[("ENABLED_SYM", "y")]);
        let conflicts = g.validate_disabled(&disabled, &v);
        assert!(conflicts.contains("FOO"));
    }

    #[test]
    fn validate_disabled_detects_conditional_conflict() {
        // BAR selects FOO if COND; COND is active → conflict
        let g = graph_from(vec![
            sym("BAR", vec![("FOO", ConfigExpr::Symbol("COND".into()))]),
            sym("FOO", vec![]),
        ]);
        let disabled: HashSet<String> = ["FOO".to_owned()].into();
        let v = vals(&[("BAR", "y"), ("COND", "y")]);
        let conflicts = g.validate_disabled(&disabled, &v);
        assert!(conflicts.contains("FOO"));
    }

    #[test]
    fn validate_disabled_no_conflict_when_selector_also_disabled() {
        let g = graph_from(vec![
            sym("BAR", vec![("FOO", ConfigExpr::Always)]),
            sym("FOO", vec![]),
        ]);
        let disabled: HashSet<String> = ["FOO".to_owned(), "BAR".to_owned()].into();
        let v = vals(&[("BAR", "y")]);
        let conflicts = g.validate_disabled(&disabled, &v);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn validate_disabled_no_conflict_when_selector_disabled_in_config() {
        // BAR unconditionally selects FOO, but BAR itself is disabled (n) → no conflict
        let g = graph_from(vec![
            sym("BAR", vec![("FOO", ConfigExpr::Always)]),
            sym("FOO", vec![]),
        ]);
        let disabled: HashSet<String> = ["FOO".to_owned()].into();
        let v = vals(&[("BAR", "n")]);
        let conflicts = g.validate_disabled(&disabled, &v);
        assert!(conflicts.is_empty());
    }
}
