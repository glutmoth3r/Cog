//! KDL syntax and strict, purpose-specific configuration schemas.
use crate::diagnostic::{Diagnostic, SourceFile, SourceSpan};
use kdl::{KdlDocument, KdlNode};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const MAX_CONFIGURATION_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub kind: PackageKind,
    pub plugins: Vec<crate::sdk::PluginManifest>,
    pub assets: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PackageKind {
    #[default]
    Application,
    Library,
}

#[derive(Clone, Debug)]
pub struct Dependency {
    pub alias: String,
    pub path: PathBuf,
    pub archive: bool,
    /// Exact package identity resolved from the user-local Cog store.
    pub installed: Option<(String, String)>,
    pub span: SourceSpan,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub name: String,
    pub entry: PathBuf,
}

pub fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "." | "..")
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
}

pub fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn configuration_size_error() -> Vec<Diagnostic> {
    vec![Diagnostic::new(
        "E3002",
        "compiler configuration byte limit (256 KiB) exceeded",
        None,
    )]
}

pub(crate) fn read_configuration(path: &Path) -> Result<String, Vec<Diagnostic>> {
    let read =
        || crate::limits::read_at_most(std::fs::File::open(path)?, MAX_CONFIGURATION_BYTES + 1);
    let read_error = |e| {
        vec![Diagnostic::new(
            "E3001",
            format!("cannot read {}: {e}", path.display()),
            None,
        )]
    };
    let bytes = read().map_err(read_error)?;
    if bytes.len() > MAX_CONFIGURATION_BYTES {
        return Err(configuration_size_error());
    }
    String::from_utf8(bytes)
        .map_err(|e| read_error(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

fn document(path: &Path, text: &str) -> Result<(Rc<SourceFile>, KdlDocument), Vec<Diagnostic>> {
    if text.len() > MAX_CONFIGURATION_BYTES {
        return Err(configuration_size_error());
    }
    let source = SourceFile::new(path.display().to_string(), text);
    if let Err(message) = crate::limits::check_kdl_depth(&source.text) {
        return Err(vec![Diagnostic::new(
            "E3002",
            message,
            Some(source.span(0..0)),
        )]);
    }
    match KdlDocument::parse_v2(&source.text) {
        Ok(doc) => Ok((source, doc)),
        Err(error) => Err(error
            .diagnostics
            .iter()
            .map(|d| {
                let mut diag = Diagnostic::new(
                    "E3002",
                    d.to_string(),
                    Some(source.span(d.span.offset()..d.span.offset() + d.span.len())),
                );
                if let Some(help) = &d.help {
                    diag = diag.note(help);
                }
                diag
            })
            .collect()),
    }
}

fn error(
    errors: &mut Vec<Diagnostic>,
    source: &Rc<SourceFile>,
    node: &KdlNode,
    message: impl Into<String>,
) {
    let s = node.span();
    errors.push(Diagnostic::new(
        "E3003",
        message,
        Some(source.span(s.offset()..s.offset() + s.len())),
    ));
}

/// Collect entries without using KDL's last-property-wins lookup.
fn fields(
    node: &KdlNode,
    allowed: &[&str],
    source: &Rc<SourceFile>,
    errors: &mut Vec<Diagnostic>,
) -> (Vec<String>, BTreeMap<String, String>) {
    fields_with_children(node, allowed, source, errors, false)
}

fn fields_with_children(
    node: &KdlNode,
    allowed: &[&str],
    source: &Rc<SourceFile>,
    errors: &mut Vec<Diagnostic>,
    allow_children: bool,
) -> (Vec<String>, BTreeMap<String, String>) {
    if node.ty().is_some() || (!allow_children && node.children().is_some()) {
        error(
            errors,
            source,
            node,
            if allow_children {
                "type annotations are not allowed here"
            } else {
                "type annotations and child blocks are not allowed here"
            },
        );
    }
    let mut args = Vec::new();
    let mut props = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for entry in node.entries() {
        let span = entry.span();
        let location = Some(source.span(span.offset()..span.offset() + span.len()));
        if entry.ty().is_some() {
            errors.push(Diagnostic::new(
                "E3003",
                "type annotations are not allowed here",
                location.clone(),
            ));
        }
        let value = entry.value().as_string();
        if let Some(name) = entry.name().map(|n| n.value()) {
            if !allowed.contains(&name) {
                errors.push(Diagnostic::new(
                    "E3003",
                    format!("unknown property `{name}`"),
                    location.clone(),
                ));
            }
            if !seen.insert(name.to_owned()) {
                errors.push(Diagnostic::new(
                    "E3003",
                    format!("duplicate property `{name}`"),
                    location.clone(),
                ));
            }
            if let Some(value) = value {
                props.insert(name.into(), value.into());
            }
        } else if let Some(value) = value {
            args.push(value.into());
        }
        if value.is_some_and(|s| {
            s.chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        }) {
            errors.push(Diagnostic::new(
                "E3003",
                "unsupported control character in string",
                location.clone(),
            ));
        }
        if value.is_none() {
            errors.push(Diagnostic::new(
                "E3003",
                "expected a string value",
                location,
            ));
        }
    }
    (args, props)
}

pub fn package(path: &Path) -> Result<Package, Vec<Diagnostic>> {
    package_from_source(path, &read_configuration(path)?)
}

/// Parse a package schema without filesystem access. The path labels diagnostics.
pub fn package_from_source(path: &Path, text: &str) -> Result<Package, Vec<Diagnostic>> {
    let (source, doc) = document(path, text)?;
    let mut errors = Vec::new();
    let mut result = None;
    for node in doc.nodes() {
        if node.name().value() != "package" {
            error(
                &mut errors,
                &source,
                node,
                format!("unknown node `{}`; expected package", node.name()),
            );
            continue;
        }
        let (args, props) =
            fields_with_children(node, &["version", "kind"], &source, &mut errors, true);
        if result.is_some() {
            error(&mut errors, &source, node, "duplicate package declaration");
        }
        if args.len() != 1
            || args.first().is_none_or(|v| v.is_empty())
            || props.get("version").is_none_or(|v| v.is_empty())
        {
            error(
                &mut errors,
                &source,
                node,
                "expected: package \"name\" version=\"version\"",
            );
        } else if !valid_package_name(&args[0]) {
            error(
                &mut errors,
                &source,
                node,
                "package name must be a filename component without separators or control characters",
            );
        } else {
            let kind = match props.get("kind").map(String::as_str) {
                None | Some("application") => PackageKind::Application,
                Some("library") => PackageKind::Library,
                Some(_) => {
                    error(
                        &mut errors,
                        &source,
                        node,
                        "package kind must be `application` or `library`",
                    );
                    continue;
                }
            };
            let mut plugins = Vec::new();
            let mut assets = Vec::new();
            let mut seen_assets = BTreeSet::new();
            if let Some(children) = node.children() {
                for child in children.nodes() {
                    match child.name().value() {
                        "plugin" => match crate::sdk::PluginManifest::parse_package_node(child) {
                            Ok(plugin) => plugins.push(plugin),
                            Err(plugin_error) => error(
                                &mut errors,
                                &source,
                                child,
                                format!("invalid package plugin declaration: {plugin_error}"),
                            ),
                        },
                        "asset" => match package_asset(child) {
                            Ok(asset) if seen_assets.insert(asset.clone()) => assets.push(asset),
                            Ok(_) => error(
                                &mut errors,
                                &source,
                                child,
                                "duplicate package asset declaration",
                            ),
                            Err(message) => error(&mut errors, &source, child, message),
                        },
                        name => error(
                            &mut errors,
                            &source,
                            child,
                            format!("unknown package child `{name}`; expected plugin or asset"),
                        ),
                    }
                }
            }
            result = Some(Package {
                name: args[0].clone(),
                version: props["version"].clone(),
                kind,
                plugins,
                assets,
            });
        }
    }
    if result.is_none() && errors.is_empty() {
        errors.push(Diagnostic::new(
            "E3003",
            "missing package declaration",
            Some(source.span(0..0)),
        ));
    }
    if errors.is_empty() {
        Ok(result.expect("validated package"))
    } else {
        Err(errors)
    }
}

fn package_asset(node: &KdlNode) -> Result<PathBuf, String> {
    if node.ty().is_some() || node.children().is_some() || node.entries().len() != 1 {
        return Err("asset requires one relative package path and no properties".into());
    }
    let entry = &node.entries()[0];
    if entry.name().is_some() || entry.ty().is_some() {
        return Err("expected: asset \"relative/path\"".into());
    }
    let value = entry
        .value()
        .as_string()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "asset requires a nonempty string path".to_owned())?;
    if value
        .chars()
        .any(|character| character.is_control() || matches!(character, '\\' | ':'))
    {
        return Err("asset path contains an unsupported character".into());
    }
    let path = PathBuf::from(value);
    if path.is_absolute()
        || value.split('/').any(|part| matches!(part, "" | "." | ".."))
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("asset path must be normalized and remain inside the package".into());
    }
    Ok(path)
}

pub fn dependencies(path: &Path) -> Result<Vec<Dependency>, Vec<Diagnostic>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    dependencies_from_source(path, &read_configuration(path)?)
}

/// Parse dependency declarations without opening or resolving their paths.
pub fn dependencies_from_source(
    path: &Path,
    text: &str,
) -> Result<Vec<Dependency>, Vec<Diagnostic>> {
    let (source, doc) = document(path, text)?;
    let mut errors = Vec::new();
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for node in doc.nodes() {
        let alias = node.name().value();
        let (args, props) = fields(
            node,
            &["path", "archive", "package", "version"],
            &source,
            &mut errors,
        );
        if !identifier(alias) || !seen.insert(alias.to_owned()) {
            error(
                &mut errors,
                &source,
                node,
                format!("invalid or duplicate dependency alias `{alias}`"),
            );
        }
        let directory = props.get("path");
        let archive = props.get("archive");
        let package = props.get("package");
        let version = props.get("version");
        let dependency_path = directory.or(archive);
        let local_source = (directory.is_some() ^ archive.is_some())
            && package.is_none()
            && version.is_none()
            && dependency_path.is_some_and(|path| !path.is_empty());
        let installed_source = directory.is_none()
            && archive.is_none()
            && package.is_some_and(|name| valid_package_name(name))
            && version.is_some_and(|value| !value.is_empty());
        if !args.is_empty() || !(local_source || installed_source) {
            error(
                &mut errors,
                &source,
                node,
                "expected alias path=\"directory\", alias archive=\"file.cogpkg\", or alias package=\"name\" version=\"version\"",
            );
        } else {
            let s = node.span();
            result.push(Dependency {
                alias: alias.into(),
                path: dependency_path.map(PathBuf::from).unwrap_or_default(),
                archive: archive.is_some(),
                installed: package
                    .zip(version)
                    .map(|(name, version)| (name.to_owned(), version.to_owned())),
                span: source.span(s.offset()..s.offset() + s.len()),
            });
        }
    }
    if errors.is_empty() {
        Ok(result)
    } else {
        Err(errors)
    }
}

pub fn targets(path: &Path) -> Result<Vec<Target>, Vec<Diagnostic>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    targets_from_source(path, &read_configuration(path)?)
}

/// Parse build targets without opening source files or creating artifacts.
pub fn targets_from_source(path: &Path, text: &str) -> Result<Vec<Target>, Vec<Diagnostic>> {
    let (source, doc) = document(path, text)?;
    let mut errors = Vec::new();
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for node in doc.nodes() {
        let (args, props) = fields(node, &["entry"], &source, &mut errors);
        if node.name().value() != "target"
            || args.len() != 1
            || args.first().is_none_or(|v| !identifier(v))
            || props.get("entry").is_none_or(|p| p.is_empty())
        {
            error(
                &mut errors,
                &source,
                node,
                "expected: target \"name\" entry=\"relative/file.cog\"",
            );
        } else if !seen.insert(args[0].clone()) {
            error(
                &mut errors,
                &source,
                node,
                format!("duplicate target `{}`", args[0]),
            );
        } else {
            let entry = PathBuf::from(&props["entry"]);
            if entry.is_absolute()
                || entry
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                error(
                    &mut errors,
                    &source,
                    node,
                    "target entry must be inside the package",
                );
            } else {
                result.push(Target {
                    name: args[0].clone(),
                    entry,
                });
            }
        }
    }
    if errors.is_empty() {
        Ok(result)
    } else {
        Err(errors)
    }
}
