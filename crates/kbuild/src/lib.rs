use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub use kconfig::{ConfigExpr, KconfigGraph};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse Makefile at {path}: {reason}")]
    ParseError { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct KernelModule {
    pub name: String,
    pub module_config: String,
    pub files: Vec<(PathBuf, ConfigExpr)>,
}

impl KernelModule {
    pub fn has_no_coverage(&self, covered: &HashSet<PathBuf>) -> bool {
        !self.files.iter().any(|(p, _)| covered.contains(p))
    }

    pub fn compiled_files<'a>(
        &'a self,
        config_values: &HashMap<&str, &str>,
    ) -> Vec<&'a PathBuf> {
        self.files
            .iter()
            .filter(|(_, expr)| expr.eval_bool(config_values))
            .map(|(p, _)| p)
            .collect()
    }
}

pub struct KbuildMaps {
    modules: HashMap<String, KernelModule>,
    config_to_files: HashMap<String, HashSet<PathBuf>>,
    file_to_expr: HashMap<PathBuf, ConfigExpr>,
}

impl KbuildMaps {
    pub fn load(kernel_src: &Path) -> Result<Self> {
        let mut builder = KbuildBuilder::new(kernel_src.to_path_buf());
        builder.walk_dir(kernel_src, &ConfigExpr::Always)?;
        Ok(builder.finish())
    }

    pub fn module(&self, name: &str) -> Option<&KernelModule> {
        self.modules.get(name)
    }

    pub fn modules(&self) -> &HashMap<String, KernelModule> {
        &self.modules
    }

    pub fn config_to_files(&self) -> &HashMap<String, HashSet<PathBuf>> {
        &self.config_to_files
    }

    pub fn file_to_expr(&self) -> &HashMap<PathBuf, ConfigExpr> {
        &self.file_to_expr
    }

    pub fn configs_safe_to_disable(&self, uncovered: &HashSet<PathBuf>) -> HashSet<String> {
        self.config_to_files
            .iter()
            .filter(|(_, files)| files.is_subset(uncovered))
            .map(|(cfg, _)| cfg.clone())
            .collect()
    }

    #[cfg(test)]
    fn for_test(
        modules: HashMap<String, KernelModule>,
        config_to_files: HashMap<String, HashSet<PathBuf>>,
        file_to_expr: HashMap<PathBuf, ConfigExpr>,
    ) -> Self {
        Self { modules, config_to_files, file_to_expr }
    }
}

/// Parses a kernel `.config` file.
/// `CONFIG_FOO=y` → `("FOO", "y")`;  `# CONFIG_FOO is not set` → `("FOO", "n")`.
pub fn parse_dotconfig(path: &Path) -> Result<HashMap<String, String>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("CONFIG_") {
            if let Some(eq) = rest.find('=') {
                let name = rest[..eq].to_owned();
                let val = rest[eq + 1..].trim_matches('"').to_owned();
                map.insert(name, val);
            }
        } else if let Some(rest) = line.strip_prefix("# CONFIG_") {
            if let Some(s) = rest.strip_suffix(" is not set") {
                map.insert(s.to_owned(), "n".to_owned());
            }
        }
    }
    Ok(map)
}

/// A single-symbol change that silences a module, with its side effects.
pub struct DisableVariant {
    /// `(symbol_name, "n")`
    pub change: (String, String),
    /// Symbols that unconditionally `select` the disabled target (must also be
    /// disabled; they would force the target back to `y` regardless of config).
    pub cascade_unconditional: HashSet<String>,
    /// Symbols whose `select TARGET if COND` is currently active (COND is true
    /// in the reference config).  Must also be disabled to prevent
    /// `make olddefconfig` from silently re-enabling the target.
    pub cascade_active: HashSet<String>,
    /// Source files outside the target module that would stop compiling.
    pub side_effect_files: HashSet<PathBuf>,
    /// Other module names that become disabled because their `module_config`
    /// symbol transitively depends on the disabled target (or its cascade).
    /// Sorted alphabetically.
    pub collateral_modules: Vec<String>,
}

impl DisableVariant {
    /// All cascade symbols (union of unconditional and conditionally-active).
    pub fn cascade_configs(&self) -> impl Iterator<Item = &str> {
        self.cascade_unconditional
            .iter()
            .chain(self.cascade_active.iter())
            .map(|s| s.as_str())
    }

    /// Total number of distinct cascade symbols.
    pub fn cascade_len(&self) -> usize {
        self.cascade_unconditional
            .union(&self.cascade_active)
            .count()
    }
}

/// Returns all single-symbol variants that silence `module` under `config_values`,
/// sorted ascending by total side-effect count (files + cascade configs).
pub fn generate_disable_variants(
    kbuild: &KbuildMaps,
    kconfig: &KconfigGraph,
    module: &KernelModule,
    config_values: &HashMap<&str, &str>,
) -> Vec<DisableVariant> {
    let module_files: HashSet<PathBuf> = module.files.iter().map(|(p, _)| p.clone()).collect();

    // Collect all currently-enabled CONFIG symbols that appear in the module's
    // file conditions (plus the module_config itself).
    let mut candidate_configs: HashSet<String> = HashSet::new();
    candidate_configs.insert(module.module_config.clone());
    for (_, expr) in &module.files {
        for sym in expr.symbols() {
            if config_values.get(sym).copied().map(|v| v == "y" || v == "m").unwrap_or(false) {
                candidate_configs.insert(sym.to_owned());
            }
        }
    }

    let mut variants = Vec::new();

    for config in &candidate_configs {
        // Build a modified config with this symbol set to "n"
        let mut modified: HashMap<&str, &str> = config_values.clone();
        modified.insert(config.as_str(), "n");

        // Check that every module file evaluates to false
        let silences_all = module
            .files
            .iter()
            .all(|(_, expr)| !expr.eval_bool(&modified));

        if !silences_all {
            continue;
        }

        // Config-aware cascade: includes both unconditional and active-conditional selectors
        let cascade = kconfig.disable_cascade(config, config_values);
        let (cascade_unconditional, cascade_active) =
            kconfig.classify_cascade(config, &cascade, config_values);

        let full_disabled: HashSet<String> = std::iter::once(config.to_owned())
            .chain(cascade.iter().cloned())
            .collect();

        // Side-effect files: files controlled by any of the disabled configs,
        // minus the module's own files
        let mut side_files: HashSet<PathBuf> = HashSet::new();
        for cfg in &full_disabled {
            if let Some(files) = kbuild.config_to_files.get(cfg.as_str()) {
                for f in files {
                    if !module_files.contains(f) {
                        side_files.insert(f.clone());
                    }
                }
            }
        }

        // Collateral modules: other modules whose module_config becomes disabled
        // via the transitive depends-on chain from full_disabled.
        let transitive = kconfig.transitive_depends_disabled(&full_disabled, config_values);
        let mut collateral_modules: Vec<String> = kbuild
            .modules()
            .iter()
            .filter(|(mod_name, m)| {
                *mod_name != &module.name
                    && transitive.contains(&m.module_config)
                    && matches!(
                        config_values.get(m.module_config.as_str()).copied(),
                        Some("y") | Some("m")
                    )
            })
            .map(|(name, _)| name.clone())
            .collect();
        collateral_modules.sort();

        variants.push(DisableVariant {
            change: (config.clone(), "n".to_owned()),
            cascade_unconditional,
            cascade_active,
            side_effect_files: side_files,
            collateral_modules,
        });
    }

    // Sort: fewest collateral modules first, then fewest side-effect files + cascade.
    variants.sort_by_key(|v| (v.collateral_modules.len(), v.side_effect_files.len() + v.cascade_len()));
    variants
}

// ── Makefile / Kbuild parser ──────────────────────────────────────────────────

struct KbuildBuilder {
    kernel_src: PathBuf,
    // module_name → (module_config, Vec<(rel_path, expr)>)
    modules: HashMap<String, (String, Vec<(PathBuf, ConfigExpr)>)>,
    config_to_files: HashMap<String, HashSet<PathBuf>>,
    file_to_expr: HashMap<PathBuf, ConfigExpr>,
    visited: HashSet<PathBuf>,
}

impl KbuildBuilder {
    fn new(kernel_src: PathBuf) -> Self {
        Self {
            kernel_src,
            modules: HashMap::new(),
            config_to_files: HashMap::new(),
            file_to_expr: HashMap::new(),
            visited: HashSet::new(),
        }
    }

    fn finish(self) -> KbuildMaps {
        let modules = self
            .modules
            .into_iter()
            .map(|(name, (module_config, files))| {
                (name.clone(), KernelModule { name, module_config, files })
            })
            .collect();
        KbuildMaps {
            modules,
            config_to_files: self.config_to_files,
            file_to_expr: self.file_to_expr,
        }
    }

    fn walk_dir(&mut self, dir: &Path, parent_cond: &ConfigExpr) -> Result<()> {
        // Try "Kbuild" first, then "Makefile"
        let kbuild_path = dir.join("Kbuild");
        let makefile_path = dir.join("Makefile");
        let path = if kbuild_path.exists() {
            kbuild_path
        } else if makefile_path.exists() {
            makefile_path
        } else {
            return Ok(());
        };

        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !self.visited.insert(canon) {
            return Ok(());
        }

        let text = std::fs::read_to_string(&path).map_err(|e| Error::Io {
            path: path.clone(),
            source: e,
        })?;

        self.parse_makefile(&path, dir, &text, parent_cond)
    }

    fn parse_makefile(
        &mut self,
        _path: &Path,
        dir: &Path,
        text: &str,
        parent_cond: &ConfigExpr,
    ) -> Result<()> {
        // Join line continuations
        let lines = join_continuations(text);

        // Pass 1: collect `obj-$(CONFIG_X) += name.o` and `obj-y += name.o`
        // Pass 2: collect per-module file additions: `name-y += f.o` / `name-$(CONFIG_Y) += f.o`
        // Pass 3: collect subdirectory recursion: `obj-$(CONFIG_X) += subdir/`

        // module_name → (gate_config, gate_expr)
        let mut module_gates: HashMap<String, (String, ConfigExpr)> = HashMap::new();
        // module_name → Vec<(file, extra_expr)>
        let mut module_files: HashMap<String, Vec<(PathBuf, ConfigExpr)>> = HashMap::new();
        // Subdirectories to recurse into: (subdir_path, condition)
        let mut subdirs: Vec<(PathBuf, ConfigExpr)> = Vec::new();

        for line in &lines {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Parse assignments `LHS += RHS` or `LHS := RHS` or `LHS = RHS`
            let (lhs, rhs, _op) = if let Some(i) = line.find("+=") {
                (&line[..i], &line[i + 2..], "+=")
            } else if let Some(i) = line.find(":=") {
                (&line[..i], &line[i + 2..], ":=")
            } else if let Some(i) = line.find('=') {
                // Don't pick up `!=` or `==`
                let prev = if i > 0 { line.as_bytes()[i - 1] } else { 0 };
                if prev == b'!' || prev == b'<' || prev == b'>' {
                    continue;
                }
                (&line[..i], &line[i + 1..], "=")
            } else {
                continue;
            };

            let lhs = lhs.trim();
            let rhs = rhs.trim();

            // `obj-$(CONFIG_X) += ...` or `obj-y += ...`
            if let Some(gate_sym) = parse_obj_prefix(lhs) {
                let gate_expr = gate_to_expr(&gate_sym, parent_cond);
                for token in rhs.split_whitespace() {
                    if token.ends_with('/') {
                        // Subdirectory
                        let subdir = dir.join(token.trim_end_matches('/'));
                        subdirs.push((subdir, gate_expr.clone()));
                    } else if let Some(mod_name) = token.strip_suffix(".o") {
                        let mod_name = mod_name.to_owned();
                        // The module name in /proc/modules uses hyphens→underscores
                        let proc_name = mod_name.replace('-', "_");
                        module_gates
                            .entry(proc_name.clone())
                            .or_insert_with(|| (gate_sym.clone(), gate_expr.clone()));
                        // A plain `obj-$(CONFIG_X) += name.o` also means
                        // name.c (or name/ subdirectory) is compiled.
                        // We record name.c as a file of this module.
                        let src = dir.join(format!("{mod_name}.c"));
                        if src.exists() || true {
                            // Always record it; parser consumers can filter
                            let rel = src.strip_prefix(&self.kernel_src)
                                .unwrap_or(&src)
                                .to_path_buf();
                            module_files
                                .entry(proc_name)
                                .or_default()
                                .push((rel, gate_expr.clone()));
                        }
                    }
                }
                continue;
            }

            // `modname-y += f.o` or `modname-$(CONFIG_Y) += f.o`
            if let Some((mod_name, extra_sym)) = parse_module_file_prefix(lhs) {
                let extra_expr: ConfigExpr = if extra_sym == "y" || extra_sym == "m" {
                    ConfigExpr::Always
                } else if extra_sym.starts_with("CONFIG_") {
                    ConfigExpr::Symbol(extra_sym[7..].to_owned())
                } else {
                    ConfigExpr::Symbol(extra_sym)
                };

                let proc_name = mod_name.replace('-', "_");
                for token in rhs.split_whitespace() {
                    if let Some(base) = token.strip_suffix(".o") {
                        let src = dir.join(format!("{base}.c"));
                        let rel = src.strip_prefix(&self.kernel_src)
                            .unwrap_or(&src)
                            .to_path_buf();
                        // Full condition = gate AND extra
                        let gate_expr = module_gates.get(&proc_name)
                            .map(|(_, e)| e.clone())
                            .unwrap_or(parent_cond.clone());
                        let full_expr = and_exprs(gate_expr, extra_expr.clone());
                        module_files
                            .entry(proc_name.clone())
                            .or_default()
                            .push((rel, full_expr));
                    }
                }
            }
        }

        // Commit modules
        for (proc_name, (gate_sym, _gate_expr)) in module_gates {
            let files = module_files.remove(&proc_name).unwrap_or_default();
            // Register in config_to_files
            for (f, _) in &files {
                self.config_to_files
                    .entry(gate_sym.clone())
                    .or_default()
                    .insert(f.clone());
                // Also register extra config deps
            }
            // file_to_expr
            for (f, expr) in &files {
                self.file_to_expr.entry(f.clone()).or_insert_with(|| expr.clone());
            }
            self.modules
                .entry(proc_name.clone())
                .or_insert_with(|| (gate_sym, files));
        }

        // Also handle remaining module_files (module files without explicit obj- gate in this file)
        for (proc_name, files) in module_files {
            for (f, expr) in &files {
                self.file_to_expr.entry(f.clone()).or_insert_with(|| expr.clone());
                if let Some(sym) = first_symbol(expr) {
                    self.config_to_files
                        .entry(sym)
                        .or_default()
                        .insert(f.clone());
                }
            }
            self.modules
                .entry(proc_name.clone())
                .and_modify(|(_, existing)| existing.extend(files.clone()))
                .or_insert_with(|| ("".to_owned(), files));
        }

        // Recurse subdirectories
        for (subdir, cond) in subdirs {
            if subdir.is_dir() {
                self.walk_dir(&subdir, &cond)?;
            }
        }

        Ok(())
    }
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

/// Returns the CONFIG symbol (or "y"/"m") from `obj-$(CONFIG_X)` or `obj-y` prefixes.
fn parse_obj_prefix(lhs: &str) -> Option<String> {
    let lhs = lhs.trim();
    if !lhs.starts_with("obj-") {
        return None;
    }
    let suffix = &lhs[4..];
    if suffix == "y" || suffix == "m" {
        return Some("y".to_owned());
    }
    // $(CONFIG_X)
    if let Some(inner) = suffix.strip_prefix("$(").and_then(|s| s.strip_suffix(')')) {
        let sym = inner.strip_prefix("CONFIG_").unwrap_or(inner).to_uppercase();
        return Some(sym);
    }
    None
}

/// Returns `(module_base_name, config_suffix)` from e.g. `nfsd-$(CONFIG_NFSD_V4)` → `("nfsd", "CONFIG_NFSD_V4")`.
fn parse_module_file_prefix(lhs: &str) -> Option<(String, String)> {
    let lhs = lhs.trim();
    // Must contain a hyphen followed by -y, -m, or -$(...)
    let hyph = lhs.rfind('-')?;
    let base = lhs[..hyph].to_owned();
    let suffix = &lhs[hyph + 1..];
    if suffix == "y" || suffix == "m" || suffix == "objs" {
        return Some((base, suffix.to_owned()));
    }
    if let Some(inner) = suffix.strip_prefix("$(").and_then(|s| s.strip_suffix(')')) {
        return Some((base, inner.to_owned()));
    }
    None
}

fn gate_to_expr(gate_sym: &str, parent_cond: &ConfigExpr) -> ConfigExpr {
    let sym_expr = if gate_sym == "y" || gate_sym == "m" {
        ConfigExpr::Always
    } else {
        ConfigExpr::Symbol(gate_sym.to_owned())
    };
    and_exprs(parent_cond.clone(), sym_expr)
}

fn and_exprs(a: ConfigExpr, b: ConfigExpr) -> ConfigExpr {
    match (&a, &b) {
        (ConfigExpr::Always, _) => b,
        (_, ConfigExpr::Always) => a,
        _ => ConfigExpr::And(Box::new(a), Box::new(b)),
    }
}

fn first_symbol(expr: &ConfigExpr) -> Option<String> {
    match expr {
        ConfigExpr::Symbol(s) => Some(s.clone()),
        ConfigExpr::And(a, _) => first_symbol(a),
        ConfigExpr::Or(a, _) => first_symbol(a),
        ConfigExpr::Not(a) => first_symbol(a),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals<'a>(pairs: &[(&'a str, &'a str)]) -> HashMap<&'a str, &'a str> {
        pairs.iter().copied().collect()
    }

    fn make_module(name: &str, config: &str, files: Vec<(&str, ConfigExpr)>) -> KernelModule {
        KernelModule {
            name: name.to_owned(),
            module_config: config.to_owned(),
            files: files.into_iter().map(|(p, e)| (PathBuf::from(p), e)).collect(),
        }
    }

    fn kconfig_sym(name: &str, selects: Vec<(&str, ConfigExpr)>) -> kconfig::Symbol {
        kconfig::Symbol {
            name: name.to_owned(),
            kind: kconfig::SymbolKind::Bool,
            depends_on: None,
            selects: selects.into_iter().map(|(t, c)| (t.to_owned(), c)).collect(),
            implies: vec![],
            choice_group: None,
        }
    }

    fn empty_kconfig() -> KconfigGraph {
        KconfigGraph::for_test(HashMap::new())
    }

    #[test]
    fn module_has_no_coverage_all_uncovered() {
        let m = make_module("mod", "MOD", vec![
            ("fs/mod/a.c", ConfigExpr::Symbol("MOD".into())),
            ("fs/mod/b.c", ConfigExpr::Symbol("MOD".into())),
        ]);
        let covered: HashSet<PathBuf> = HashSet::new();
        assert!(m.has_no_coverage(&covered));
    }

    #[test]
    fn module_has_no_coverage_one_covered() {
        let m = make_module("mod", "MOD", vec![
            ("fs/mod/a.c", ConfigExpr::Symbol("MOD".into())),
            ("fs/mod/b.c", ConfigExpr::Symbol("MOD".into())),
        ]);
        let covered: HashSet<PathBuf> = [PathBuf::from("fs/mod/a.c")].into();
        assert!(!m.has_no_coverage(&covered));
    }

    #[test]
    fn compiled_files_filters_by_config() {
        let m = make_module("mod", "MOD", vec![
            ("fs/mod/a.c", ConfigExpr::Symbol("MOD".into())),
            ("fs/mod/v4.c", ConfigExpr::And(
                Box::new(ConfigExpr::Symbol("MOD".into())),
                Box::new(ConfigExpr::Symbol("MOD_V4".into())),
            )),
        ]);
        let v = vals(&[("MOD", "y")]);
        let files = m.compiled_files(&v);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], &PathBuf::from("fs/mod/a.c"));

        let v2 = vals(&[("MOD", "y"), ("MOD_V4", "y")]);
        assert_eq!(m.compiled_files(&v2).len(), 2);
    }

    #[test]
    fn configs_safe_to_disable_respects_shared_files() {
        let uncovered: HashSet<PathBuf> = [
            PathBuf::from("fs/mod/a.c"),
            PathBuf::from("fs/mod/b.c"),
        ].into();

        let mut c2f: HashMap<String, HashSet<PathBuf>> = HashMap::new();
        // MOD only controls mod files → safe
        c2f.insert("MOD".to_owned(), [PathBuf::from("fs/mod/a.c"), PathBuf::from("fs/mod/b.c")].into());
        // SHARED controls a mod file AND an outside file → not safe
        c2f.insert("SHARED".to_owned(), [PathBuf::from("fs/mod/a.c"), PathBuf::from("net/foo.c")].into());

        let maps = KbuildMaps::for_test(HashMap::new(), c2f, HashMap::new());
        let safe = maps.configs_safe_to_disable(&uncovered);
        assert!(safe.contains("MOD"));
        assert!(!safe.contains("SHARED"));
    }

    #[test]
    fn disable_plan_simple_no_cascade() {
        let m = make_module("mod", "MOD", vec![
            ("fs/mod/a.c", ConfigExpr::Symbol("MOD".into())),
        ]);
        let mut c2f: HashMap<String, HashSet<PathBuf>> = HashMap::new();
        c2f.insert("MOD".to_owned(), [PathBuf::from("fs/mod/a.c")].into());
        let maps = KbuildMaps::for_test(HashMap::new(), c2f, HashMap::new());
        let kconfig = empty_kconfig();
        let cv = vals(&[("MOD", "y")]);
        let variants = generate_disable_variants(&maps, &kconfig, &m, &cv);
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].change, ("MOD".to_owned(), "n".to_owned()));
        assert_eq!(variants[0].cascade_len(), 0);
        assert!(variants[0].side_effect_files.is_empty());
    }

    #[test]
    fn disable_plan_unconditional_select_cascade() {
        // FOO unconditionally selects MOD → must appear in cascade_unconditional
        let m = make_module("mod", "MOD", vec![
            ("fs/mod/a.c", ConfigExpr::Symbol("MOD".into())),
        ]);
        let mut c2f: HashMap<String, HashSet<PathBuf>> = HashMap::new();
        c2f.insert("MOD".to_owned(), [PathBuf::from("fs/mod/a.c")].into());
        let maps = KbuildMaps::for_test(HashMap::new(), c2f, HashMap::new());

        let symbols: HashMap<String, kconfig::Symbol> = [
            ("FOO".to_owned(), kconfig_sym("FOO", vec![("MOD", ConfigExpr::Always)])),
            ("MOD".to_owned(), kconfig_sym("MOD", vec![])),
        ].into_iter().collect();
        let kconfig = KconfigGraph::for_test(symbols);

        let cv = vals(&[("MOD", "y"), ("FOO", "y")]);
        let variants = generate_disable_variants(&maps, &kconfig, &m, &cv);
        assert_eq!(variants.len(), 1);
        assert!(variants[0].cascade_unconditional.contains("FOO"),
            "FOO must be in cascade_unconditional");
        assert!(!variants[0].cascade_active.contains("FOO"));
    }

    #[test]
    fn disable_plan_conditional_active_select() {
        // BAR selects MOD if NET; NET is currently y → BAR in cascade_active
        let m = make_module("mod", "MOD", vec![
            ("fs/mod/a.c", ConfigExpr::Symbol("MOD".into())),
        ]);
        let mut c2f: HashMap<String, HashSet<PathBuf>> = HashMap::new();
        c2f.insert("MOD".to_owned(), [PathBuf::from("fs/mod/a.c")].into());
        let maps = KbuildMaps::for_test(HashMap::new(), c2f, HashMap::new());

        let symbols: HashMap<String, kconfig::Symbol> = [
            ("BAR".to_owned(), kconfig_sym("BAR",
                vec![("MOD", ConfigExpr::Symbol("NET".into()))])),
            ("MOD".to_owned(), kconfig_sym("MOD", vec![])),
        ].into_iter().collect();
        let kconfig = KconfigGraph::for_test(symbols);

        let cv = vals(&[("MOD", "y"), ("BAR", "y"), ("NET", "y")]);
        let variants = generate_disable_variants(&maps, &kconfig, &m, &cv);
        assert_eq!(variants.len(), 1);
        assert!(variants[0].cascade_active.contains("BAR"),
            "BAR must be in cascade_active: select MOD if NET with NET=y");
        assert!(!variants[0].cascade_unconditional.contains("BAR"));
    }

    #[test]
    fn disable_plan_conditional_inactive_select_not_cascaded() {
        // BAR selects MOD if NET; NET is currently n → BAR must NOT appear
        let m = make_module("mod", "MOD", vec![
            ("fs/mod/a.c", ConfigExpr::Symbol("MOD".into())),
        ]);
        let mut c2f: HashMap<String, HashSet<PathBuf>> = HashMap::new();
        c2f.insert("MOD".to_owned(), [PathBuf::from("fs/mod/a.c")].into());
        let maps = KbuildMaps::for_test(HashMap::new(), c2f, HashMap::new());

        let symbols: HashMap<String, kconfig::Symbol> = [
            ("BAR".to_owned(), kconfig_sym("BAR",
                vec![("MOD", ConfigExpr::Symbol("NET".into()))])),
            ("MOD".to_owned(), kconfig_sym("MOD", vec![])),
        ].into_iter().collect();
        let kconfig = KconfigGraph::for_test(symbols);

        let cv = vals(&[("MOD", "y"), ("BAR", "y"), ("NET", "n")]);
        let variants = generate_disable_variants(&maps, &kconfig, &m, &cv);
        assert_eq!(variants.len(), 1);
        assert!(!variants[0].cascade_active.contains("BAR"));
        assert!(!variants[0].cascade_unconditional.contains("BAR"));
    }

    #[test]
    fn disable_plan_shared_config_has_side_effects() {
        let m = make_module("mod", "SHARED", vec![
            ("fs/mod/a.c", ConfigExpr::Symbol("SHARED".into())),
        ]);
        let mut c2f: HashMap<String, HashSet<PathBuf>> = HashMap::new();
        c2f.insert("SHARED".to_owned(), [
            PathBuf::from("fs/mod/a.c"),
            PathBuf::from("net/other.c"),
        ].into());
        let maps = KbuildMaps::for_test(HashMap::new(), c2f, HashMap::new());
        let kconfig = empty_kconfig();
        let cv = vals(&[("SHARED", "y")]);
        let variants = generate_disable_variants(&maps, &kconfig, &m, &cv);
        assert_eq!(variants.len(), 1);
        assert!(variants[0].side_effect_files.contains(&PathBuf::from("net/other.c")));
    }
}
