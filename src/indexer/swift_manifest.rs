//! Target and dependency extraction from Swift manifests: SwiftPM
//! `Package.swift` and Tuist `Project.swift`, parsed with tree-sitter-swift.
//!
//! Targets are call expressions inside a `targets` or `modules` array literal —
//! `.target(name: "Foo", dependencies: ["Bar"])` in SwiftPM, or project helpers
//! such as `.spmSwiftFolderTarget(name: .Foo, ...)` / `.module(name: .Foo, ...)`
//! in Tuist. The array may be an argument of `Package(...)`/`Project(...)` or a
//! `let targets = [...]` declaration passed in later.
//!
//! Manifests are not evaluated, so project helpers are read by convention:
//! the source directory is probed among common layouts, and nested
//! `implementation:` / `tests:` configurations declare `<Name>Impl` /
//! `<Name>Tests` companion targets. Targets produced by helper functions
//! outside such arrays are not discovered; [`Manifest::declares_project`]
//! lets callers flag a manifest that yielded nothing.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use tree_sitter::{Language, Node, Parser};

static SWIFT: LazyLock<Language> = LazyLock::new(|| tree_sitter_swift::LANGUAGE.into());

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub targets: Vec<ManifestTarget>,
    /// The manifest calls `Project(...)` / `Package(...)` directly, so an empty
    /// `targets` means the declarations were not understood rather than absent.
    pub declares_project: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestTarget {
    pub name: String,
    /// Candidate source directories relative to the manifest, most specific
    /// first; [`target_dir`] picks the first one that exists.
    pub dirs: Vec<String>,
    pub dependencies: Vec<ManifestDependency>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestDependency {
    pub name: String,
    /// Declaration form: `target`, `external`, `project`, `product`, ...
    pub kind: String,
}

/// Dependency forms that never refer to a source module of the workspace.
const NON_MODULE_DEPENDENCIES: &[&str] = &["sdk", "system", "xcframework", "framework", "library"];

/// Argument labels whose array holds target declarations.
const TARGET_ARRAY_LABELS: &[&str] = &["targets", "modules"];

pub fn parse_manifest(content: &str) -> Manifest {
    let mut parser = Parser::new();
    if parser.set_language(&SWIFT).is_err() {
        return Manifest::default();
    }
    let Some(tree) = parser.parse(content, None) else {
        return Manifest::default();
    };
    let root = tree.root_node();
    let mut arrays = Vec::new();
    collect_target_arrays(root, content, &mut arrays);

    let mut targets: Vec<ManifestTarget> = Vec::new();
    for element in arrays.into_iter().flat_map(named_children) {
        for target in parse_declaration(element, content) {
            if !targets.iter().any(|t| t.name == target.name) {
                targets.push(target);
            }
        }
    }
    Manifest {
        targets,
        declares_project: calls_project_constructor(root, content),
    }
}

/// Source directory of a manifest target: the first candidate that exists on
/// disk, else the most specific candidate.
pub fn target_dir(manifest_dir: &Path, target: &ManifestTarget) -> PathBuf {
    target
        .dirs
        .iter()
        .map(|dir| manifest_dir.join(dir))
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| {
            manifest_dir.join(target.dirs.first().map_or(target.name.as_str(), String::as_str))
        })
}

/// Array literals holding target declarations: a `targets:` / `modules:`
/// argument, or the value of a `let/var ...targets` / `...modules` declaration.
fn collect_target_arrays<'t>(node: Node<'t>, content: &str, out: &mut Vec<Node<'t>>) {
    let array = match node.kind() {
        "value_argument" => node
            .child_by_field_name("name")
            .filter(|label| TARGET_ARRAY_LABELS.contains(&text(*label, content)))
            .and_then(|_| node.child_by_field_name("value")),
        "property_declaration" => node
            .child_by_field_name("name")
            .filter(|name| {
                let name = text(*name, content).to_ascii_lowercase();
                TARGET_ARRAY_LABELS.iter().any(|label| name.ends_with(label))
            })
            .and_then(|_| node.child_by_field_name("value")),
        _ => None,
    };
    if let Some(array) = array.filter(|v| v.kind() == "array_literal") {
        out.push(array);
        return;
    }
    for child in named_children(node) {
        collect_target_arrays(child, content, out);
    }
}

/// A direct `Project(...)` call, or a `Package(...)` call with a `targets:`
/// argument — as opposed to a manifest built by a helper such as
/// `App.main.project(...)`, or a dependencies-only `Package(...)`.
fn calls_project_constructor(node: Node, content: &str) -> bool {
    if node.kind() == "call_expression" {
        if let Some(callee) = node.named_child(0).filter(|c| c.kind() == "simple_identifier") {
            match text(callee, content) {
                "Project" => return true,
                "Package" => {
                    let has_targets = parse_call(node, content)
                        .is_some_and(|(_, args)| labeled(&args, "targets").is_some());
                    if has_targets {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    named_children(node)
        .into_iter()
        .any(|child| calls_project_constructor(child, content))
}

/// The target a declaration names, plus companions from nested
/// `implementation:` / `tests:` configurations.
fn parse_declaration(element: Node, content: &str) -> Vec<ManifestTarget> {
    let Some((_, args)) = parse_call(element, content) else {
        return Vec::new();
    };
    let Some(name) = labeled(&args, "name").and_then(|v| simple_name(v, content)) else {
        return Vec::new();
    };

    let explicit_dir = labeled(&args, "path")
        .and_then(|v| string_literal(v, content))
        .or_else(|| {
            labeled(&args, "sources")
                .and_then(|v| first_string_literal(v, content))
                .map(|glob| static_glob_prefix(&glob))
        })
        .filter(|dir| !dir.is_empty());
    let dirs = match explicit_dir {
        Some(dir) => vec![dir],
        None => conventional_dirs(&name),
    };

    let mut targets = vec![ManifestTarget {
        dependencies: dependencies_of(&args, content),
        name: name.clone(),
        dirs,
    }];
    let companions = [
        ("implementation", "Impl", vec![format!("{name}/Impl"), format!("Sources/{name}Impl")]),
        ("tests", "Tests", vec![format!("{name}/Tests"), format!("Tests/{name}Tests")]),
    ];
    for (label, suffix, dirs) in companions {
        let Some(config) = labeled(&args, label) else {
            continue;
        };
        let dependencies = match parse_call(config, content) {
            Some((_, config_args)) => dependencies_of(&config_args, content),
            None if text(config, content) == "true" => Vec::new(),
            None => continue,
        };
        targets.push(ManifestTarget {
            name: format!("{name}{suffix}"),
            dirs,
            dependencies,
        });
    }
    targets
}

/// SwiftPM (`Sources/X`, `Tests/X`) and per-module folder (`X/Sources`,
/// `X/Api`, `X/Tests` for `XTests`) layouts.
fn conventional_dirs(name: &str) -> Vec<String> {
    let mut dirs = vec![
        format!("Sources/{name}"),
        format!("{name}/Sources"),
        format!("{name}/Api"),
        format!("Tests/{name}"),
    ];
    if let Some(stem) = name.strip_suffix("Tests").filter(|s| !s.is_empty()) {
        dirs.push(format!("{stem}/Tests"));
    }
    dirs.push(name.to_string());
    dirs
}

fn dependencies_of(args: &Args, content: &str) -> Vec<ManifestDependency> {
    labeled(args, "dependencies")
        .filter(|v| v.kind() == "array_literal")
        .map(|deps| {
            named_children(deps)
                .into_iter()
                .filter_map(|dep| parse_dependency(dep, content))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_dependency(element: Node, content: &str) -> Option<ManifestDependency> {
    if let Some(name) = string_literal(element, content) {
        return Some(ManifestDependency {
            name,
            kind: "target".to_string(),
        });
    }
    let (callee, args) = parse_call(element, content)?;
    if NON_MODULE_DEPENDENCIES.contains(&callee.as_str()) {
        return None;
    }
    // `.project(Other.self, target: .Foo)` names the project positionally and the
    // target by label, so the `target:` label wins over positional arguments.
    let name = labeled(&args, "target")
        .or_else(|| labeled(&args, "name"))
        .or_else(|| args.iter().find(|(label, _)| label.is_none()).map(|(_, v)| *v))
        .and_then(|v| simple_name(v, content))?;
    Some(ManifestDependency { name, kind: callee })
}

type Args<'t> = Vec<(Option<String>, Node<'t>)>;

/// Callee name (`target` for `.target(...)`, `Target.target(...)`, `target(...)`)
/// and the labeled/positional arguments of a call expression.
fn parse_call<'t>(node: Node<'t>, content: &str) -> Option<(String, Args<'t>)> {
    if node.kind() != "call_expression" {
        return None;
    }
    let callee = node.named_child(0)?;
    let callee = simple_name(callee, content)?;
    let suffix = named_children(node)
        .into_iter()
        .find(|c| c.kind() == "call_suffix")?;
    let arguments = named_children(suffix)
        .into_iter()
        .find(|c| c.kind() == "value_arguments")?;
    let args = named_children(arguments)
        .into_iter()
        .filter(|arg| arg.kind() == "value_argument")
        .filter_map(|arg| {
            let value = arg.child_by_field_name("value")?;
            let label = arg
                .child_by_field_name("name")
                .map(|label| text(label, content).to_string());
            Some((label, value))
        })
        .collect();
    Some((callee, args))
}

fn labeled<'t>(args: &Args<'t>, label: &str) -> Option<Node<'t>> {
    args.iter()
        .find(|(l, _)| l.as_deref() == Some(label))
        .map(|(_, value)| *value)
}

/// `"Foo"`, `.Foo`, `Foo`, `Foo.self`, or `Namespace.Foo` → `Foo`.
fn simple_name(node: Node, content: &str) -> Option<String> {
    match node.kind() {
        "line_string_literal" => string_literal(node, content),
        "simple_identifier" => Some(text(node, content).to_string()),
        "prefix_expression" => node
            .child_by_field_name("target")
            .filter(|t| t.kind() == "simple_identifier")
            .map(|t| text(t, content).to_string()),
        "navigation_expression" => {
            let suffix = node
                .child_by_field_name("suffix")?
                .child_by_field_name("suffix")?;
            match text(suffix, content) {
                "self" => simple_name(node.child_by_field_name("target")?, content),
                name => Some(name.to_string()),
            }
        }
        _ => None,
    }
}

/// Text of a plain string literal; `None` when it contains interpolation.
fn string_literal(node: Node, content: &str) -> Option<String> {
    if node.kind() != "line_string_literal" {
        return None;
    }
    let parts = named_children(node);
    match parts.as_slice() {
        [part] if part.kind() == "line_str_text" => Some(text(*part, content).to_string()),
        _ => None,
    }
}

fn first_string_literal(node: Node, content: &str) -> Option<String> {
    string_literal(node, content).or_else(|| {
        named_children(node)
            .into_iter()
            .find_map(|child| first_string_literal(child, content))
    })
}

fn named_children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn text<'a>(node: Node, content: &'a str) -> &'a str {
    node.utf8_text(content.as_bytes()).unwrap_or("")
}

/// `Sources/Foo/**/*.swift` → `Sources/Foo`.
fn static_glob_prefix(glob: &str) -> String {
    let cut = glob.find(['*', '{', '?', '[']).unwrap_or(glob.len());
    let prefix = &glob[..cut];
    let prefix = match prefix.rfind('/') {
        Some(slash) if cut < glob.len() => &prefix[..slash],
        _ => prefix,
    };
    prefix.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(name: &str, kind: &str) -> ManifestDependency {
        ManifestDependency {
            name: name.to_string(),
            kind: kind.to_string(),
        }
    }

    fn names(manifest: &Manifest) -> Vec<&str> {
        manifest.targets.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn parses_swiftpm_targets_and_dependencies() {
        let manifest = parse_manifest(
            r#"
// swift-tools-version:5.9
let package = Package(
    name: "Core",
    products: [.library(name: "Core", targets: ["Core", "Utils"])],
    dependencies: [.package(path: "../Other")],
    targets: [
        .target(name: "Core", dependencies: ["Utils", .product(name: "Other", package: "Other")]),
        .target(name: "Utils", path: "Lib/Utils"), // trailing comment, with comma
        /* .target(name: "Commented"), */
        .testTarget(name: "CoreTests", dependencies: [.target(name: "Core")]),
    ]
)
"#,
        );
        assert!(manifest.declares_project);
        assert_eq!(names(&manifest), ["Core", "Utils", "CoreTests"]);
        let targets = &manifest.targets;
        assert_eq!(
            targets[0].dependencies,
            [dep("Utils", "target"), dep("Other", "product")]
        );
        assert_eq!(targets[1].dirs, ["Lib/Utils"]);
        assert_eq!(targets[2].dependencies, [dep("Core", "target")]);
    }

    #[test]
    fn parses_tuist_helper_targets_with_enum_names() {
        let manifest = parse_manifest(
            r#"
let project = Project(
    name: Core.self,
    targets: [
        .spmSwiftFolderTarget(
            name: .YandexGoBaseRouting,
            dependencies: [
                .external(name: "YandexGoFoundation"),
                .target(.YandexGoMapViewController),
                .project(Application.self, target: .FLEXWrapper, status: .optional),
                .system(.MapKit),
            ]
        ),
        .spmUnitTestsFolderTarget(name: .YandexGoBaseRoutingTests),
        .target(name: "App", destinations: .iOS, sources: ["App/Sources/**/*.swift"]),
    ]
)
"#,
        );
        assert_eq!(names(&manifest), ["YandexGoBaseRouting", "YandexGoBaseRoutingTests", "App"]);
        assert_eq!(
            manifest.targets[0].dependencies,
            [
                dep("YandexGoFoundation", "external"),
                dep("YandexGoMapViewController", "target"),
                dep("FLEXWrapper", "project"),
            ]
        );
        assert_eq!(manifest.targets[2].dirs, ["App/Sources"]);
    }

    #[test]
    fn module_helpers_declare_companion_targets() {
        let manifest = parse_manifest(
            r#"
let project = Project(
    name: Foundation.self,
    modules: [
        .module(
            name: .Cache,
            dependencies: [.external(name: "Base")],
            tests: .init(dependencies: [.target(.Cache), .target(.TestHelpers)])
        ),
        .apiImplModule(
            name: .Socket,
            dependencies: [.external(name: "Base")],
            implementation: .init(dependencies: [.target(.Socket)]),
            tests: .init(dependencies: [.target(.SocketImpl)])
        ),
        .module(name: .TestHelpers),
        .externalModuleTests(name: .BaseTests, dependencies: [.external(name: "Base")]),
    ]
)
"#,
        );
        assert_eq!(
            names(&manifest),
            ["Cache", "CacheTests", "Socket", "SocketImpl", "SocketTests", "TestHelpers", "BaseTests"]
        );
        let target = |name: &str| manifest.targets.iter().find(|t| t.name == name).unwrap();
        assert_eq!(target("Cache").dependencies, [dep("Base", "external")]);
        assert_eq!(
            target("CacheTests").dependencies,
            [dep("Cache", "target"), dep("TestHelpers", "target")]
        );
        assert_eq!(target("SocketImpl").dependencies, [dep("Socket", "target")]);
        assert_eq!(target("SocketImpl").dirs[0], "Socket/Impl");
        assert!(target("BaseTests").dirs.contains(&"Base/Tests".to_string()));
    }

    #[test]
    fn target_dir_prefers_existing_conventional_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        for existing in ["Tests/FooTests", "Bar/Sources", "Baz/Api", "Qux/Tests"] {
            std::fs::create_dir_all(dir.path().join(existing)).unwrap();
        }
        let target = |name: &str| ManifestTarget {
            name: name.to_string(),
            dirs: conventional_dirs(name),
            dependencies: vec![],
        };
        let resolved = |name: &str| target_dir(dir.path(), &target(name));
        assert_eq!(resolved("FooTests"), dir.path().join("Tests/FooTests"));
        assert_eq!(resolved("Bar"), dir.path().join("Bar/Sources"));
        assert_eq!(resolved("Baz"), dir.path().join("Baz/Api"));
        assert_eq!(resolved("QuxTests"), dir.path().join("Qux/Tests"));
        assert_eq!(resolved("Missing"), dir.path().join("Sources/Missing"));
    }

    #[test]
    fn parses_targets_declared_in_a_variable() {
        let manifest = parse_manifest(
            r#"
let targets: [PackageDescription.Target] = [
    .target(name: "Maps", dependencies: [.product(name: "Geo", package: "geo")]),
]
let documentableTargets: [String] = ["Maps"]
let package = Package(name: "Maps", targets: targets)
"#,
        );
        assert_eq!(names(&manifest), ["Maps"]);
        assert_eq!(manifest.targets[0].dependencies, [dep("Geo", "product")]);
    }

    #[test]
    fn reports_project_manifests_without_understood_targets() {
        let helper_built = parse_manifest("let project = App.main.fullProject(dependencies: [])");
        assert!(!helper_built.declares_project);
        let dependencies_only =
            parse_manifest("let package = Package(name: \"Deps\", dependencies: [.package(path: \"X\")])");
        assert!(!dependencies_only.declares_project);
        let unknown_layout =
            parse_manifest("let project = Project(name: \"X\", features: [.feature(name: .A)])");
        assert!(unknown_layout.declares_project);
        assert!(unknown_layout.targets.is_empty());
    }

    #[test]
    fn ignores_non_call_elements_and_unbalanced_input() {
        assert!(parse_manifest("let t = Target(targets: [\"A\", \"B\"])").targets.is_empty());
        assert!(parse_manifest("targets: [ .target(name: \"A\"").targets.is_empty());
    }
}
