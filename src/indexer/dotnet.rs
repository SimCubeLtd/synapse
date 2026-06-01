//! Parsing of `.csproj` MSBuild project files into the dependency facts the
//! indexer cares about: project-to-project references and NuGet package
//! references.
//!
//! `.csproj` files are XML. Rather than build a full DOM we stream events with
//! [`quick_xml`], which keeps us robust to the wide variety of hand-edited and
//! tool-generated layouts seen in the wild (self-closing vs. nested elements,
//! arbitrary attribute ordering, mixed casing of values, etc.).
//!
//! Two shapes of `PackageReference` are supported:
//!
//! ```xml
//! <!-- attribute form (most common) -->
//! <PackageReference Include="Serilog" Version="3.1.1" />
//!
//! <!-- child-element form -->
//! <PackageReference Include="Serilog">
//!   <Version>3.1.1</Version>
//! </PackageReference>
//! ```
//!
//! Ordering is preserved in document order; de-duplication is intentionally left
//! to the caller.

use anyhow::{Context, Result};
use quick_xml::Reader;
use quick_xml::events::Event;

/// Parsed dependency facts extracted from a single `.csproj` file.
#[derive(Debug, Default)]
pub struct CsprojData {
    /// Raw `Include` values of every `<ProjectReference>` (typically relative
    /// paths to other `.csproj` files), in document order.
    pub project_references: Vec<String>,
    /// Every `<PackageReference>` (NuGet dependency), in document order.
    pub package_references: Vec<PackageRef>,
    /// Every `<PackageVersion>` (Central Package Management version pin), in
    /// document order. Populated mainly from `Directory.Packages.props`, but a
    /// `.csproj`/`.props` may carry them too.
    pub package_versions: Vec<PackageRef>,
    /// Value of `<ManagedPackageVersionsCentrally>` if present (the flag that
    /// enables Central Package Management). Usually set in
    /// `Directory.Build.props` or `Directory.Packages.props`, not the `.csproj`.
    /// `Some(true)` when explicitly enabled, `Some(false)` when disabled.
    pub cpm_enabled: Option<bool>,
}

/// A single NuGet package reference.
#[derive(Debug, Clone)]
pub struct PackageRef {
    /// Package id, taken from the `Include` attribute.
    pub name: String,
    /// Package version, from the `Version` attribute or a child `<Version>`
    /// element. `None` when neither is present (e.g. central package
    /// management, where versions live in `Directory.Packages.props`).
    pub version: Option<String>,
}

/// Parse the textual contents of a `.csproj` file.
///
/// Extracts every `<ProjectReference Include="..."/>` and every
/// `<PackageReference Include="..." [Version="..."]/>`. The version of a package
/// may instead be supplied via a nested `<Version>` child element; that form is
/// also handled.
///
/// Returns an error only if the XML is malformed enough that the streaming
/// reader cannot make progress; missing attributes or unexpected elements are
/// tolerated silently.
pub fn parse_csproj(text: &str) -> Result<CsprojData> {
    let mut reader = Reader::from_str(text);
    // Be lenient: hand-edited csproj files are not always perfectly closed.
    let config = reader.config_mut();
    config.check_end_names = false;

    let mut data = CsprojData::default();

    // State for a `<PackageReference>`/`<PackageVersion>` whose version we may
    // still discover in a child `<Version>` element. `Some` while we are inside
    // such an element awaiting its close tag; the bool marks whether it is a
    // PackageVersion (true) vs a PackageReference (false).
    let mut pending_pkg: Option<(PackageRef, bool)> = None;
    // True while we are directly inside a `<Version>` child of a pending package
    // reference, so the following text event is captured as the version.
    let mut in_version_child = false;
    // True while inside `<ManagedPackageVersionsCentrally>`, so its text content
    // can be read as the CPM-enabled flag.
    let mut in_cpm_flag = false;

    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .context("failed reading MSBuild XML")?
        {
            Event::Empty(e) => {
                // Self-closing element: it has no children, so handle it wholly
                // here. A self-closing package element must carry its version
                // (if any) as an attribute.
                match e.name().as_ref() {
                    b"ProjectReference" => {
                        if let Some(inc) = find_attr(&e, b"Include") {
                            data.project_references.push(inc);
                        }
                    }
                    b"PackageReference" => {
                        if let Some(name) = find_attr(&e, b"Include") {
                            let version = find_attr(&e, b"Version");
                            data.package_references.push(PackageRef { name, version });
                        }
                    }
                    b"PackageVersion" => {
                        if let Some(name) = find_attr(&e, b"Include") {
                            let version = find_attr(&e, b"Version");
                            data.package_versions.push(PackageRef { name, version });
                        }
                    }
                    _ => {}
                }
            }
            Event::Start(e) => match e.name().as_ref() {
                b"ProjectReference" => {
                    if let Some(inc) = find_attr(&e, b"Include") {
                        data.project_references.push(inc);
                    }
                }
                b"PackageReference" => {
                    if let Some(name) = find_attr(&e, b"Include") {
                        // Capture an attribute version up front; a child
                        // `<Version>` element, if present, overrides it.
                        let version = find_attr(&e, b"Version");
                        pending_pkg = Some((PackageRef { name, version }, false));
                    }
                }
                b"PackageVersion" => {
                    if let Some(name) = find_attr(&e, b"Include") {
                        let version = find_attr(&e, b"Version");
                        pending_pkg = Some((PackageRef { name, version }, true));
                    }
                }
                b"Version"
                    // Only meaningful as a child of a pending package element.
                    if pending_pkg.is_some() => {
                        in_version_child = true;
                    }
                b"ManagedPackageVersionsCentrally" => in_cpm_flag = true,
                _ => {}
            },
            Event::Text(t) => {
                if in_version_child && let Some((pkg, _)) = pending_pkg.as_mut() {
                    let v = t
                        .decode()
                        .context("failed to decode <Version> text")?
                        .trim()
                        .to_string();
                    if !v.is_empty() {
                        pkg.version = Some(v);
                    }
                } else if in_cpm_flag {
                    let v = t
                        .decode()
                        .context("failed to decode CPM flag text")?
                        .trim()
                        .to_ascii_lowercase();
                    if !v.is_empty() {
                        data.cpm_enabled = Some(v == "true");
                    }
                }
            }
            Event::End(e) => match e.name().as_ref() {
                b"Version" => in_version_child = false,
                b"ManagedPackageVersionsCentrally" => in_cpm_flag = false,
                b"PackageReference" | b"PackageVersion" => {
                    if let Some((pkg, is_version)) = pending_pkg.take() {
                        if is_version {
                            data.package_versions.push(pkg);
                        } else {
                            data.package_references.push(pkg);
                        }
                    }
                    in_version_child = false;
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    // Defensive: if the document ended without closing a pending element (with
    // lenient end-name checking this is possible), still record it.
    if let Some((pkg, is_version)) = pending_pkg.take() {
        if is_version {
            data.package_versions.push(pkg);
        } else {
            data.package_references.push(pkg);
        }
    }

    Ok(data)
}

/// Parse any MSBuild XML file (`.csproj`, `.props`, `.targets`) for the same
/// dependency facts. `Directory.Packages.props` supplies `<PackageVersion>`
/// entries; `Directory.Build.props` may carry shared `<PackageReference>`s.
///
/// This is an alias of [`parse_csproj`] — the streaming logic is identical and
/// simply picks up whichever elements are present.
pub fn parse_msbuild(text: &str) -> Result<CsprojData> {
    parse_csproj(text)
}

/// Build a central-version map (`package name -> version`) from one or more
/// parsed MSBuild files' `package_versions`. Later entries win on conflict,
/// matching MSBuild's "nearest Directory.Packages.props" precedence when the
/// caller feeds files in farthest-to-nearest order.
pub fn central_version_map<'a>(
    sources: impl IntoIterator<Item = &'a CsprojData>,
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for data in sources {
        for pv in &data.package_versions {
            if let Some(v) = &pv.version {
                map.insert(pv.name.clone(), v.clone());
            }
        }
    }
    map
}

/// Find an attribute by its (byte) name on a start/empty element, returning the
/// unescaped value. Returns `None` if the attribute is absent or unreadable.
fn find_attr(e: &quick_xml::events::BytesStart<'_>, want: &[u8]) -> Option<String> {
    for attr in e.attributes() {
        let attr = attr.ok()?;
        if attr.key.as_ref() == want {
            // `unescape_value` is deprecated in favour of `normalized_value`, but
            // the latter's signature is awkward here; the unescape semantics are
            // exactly what we want for `Include`/`Version` attributes.
            #[allow(deprecated)]
            let val = attr.unescape_value().ok()?;
            return Some(val.into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_self_closing_references() {
        let xml = r#"
            <Project Sdk="Microsoft.NET.Sdk">
              <ItemGroup>
                <ProjectReference Include="..\Core\Core.csproj" />
                <PackageReference Include="Serilog" Version="3.1.1" />
                <PackageReference Include="Newtonsoft.Json" />
              </ItemGroup>
            </Project>
        "#;
        let d = parse_csproj(xml).unwrap();
        assert_eq!(
            d.project_references,
            vec![r"..\Core\Core.csproj".to_string()]
        );
        assert_eq!(d.package_references.len(), 2);
        assert_eq!(d.package_references[0].name, "Serilog");
        assert_eq!(d.package_references[0].version.as_deref(), Some("3.1.1"));
        assert_eq!(d.package_references[1].name, "Newtonsoft.Json");
        assert_eq!(d.package_references[1].version, None);
    }

    #[test]
    fn parses_child_version_element() {
        let xml = r#"
            <Project>
              <ItemGroup>
                <PackageReference Include="Serilog">
                  <Version>2.10.0</Version>
                </PackageReference>
              </ItemGroup>
            </Project>
        "#;
        let d = parse_csproj(xml).unwrap();
        assert_eq!(d.package_references.len(), 1);
        assert_eq!(d.package_references[0].name, "Serilog");
        assert_eq!(d.package_references[0].version.as_deref(), Some("2.10.0"));
    }

    #[test]
    fn child_version_overrides_missing_attribute() {
        let xml = r#"
            <Project>
              <ProjectReference Include="a.csproj"></ProjectReference>
              <PackageReference Include="Foo" Version="1.0.0">
                <Version>2.0.0</Version>
              </PackageReference>
            </Project>
        "#;
        let d = parse_csproj(xml).unwrap();
        assert_eq!(d.project_references, vec!["a.csproj".to_string()]);
        assert_eq!(d.package_references[0].version.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn empty_input_is_ok() {
        let d = parse_csproj("").unwrap();
        assert!(d.project_references.is_empty());
        assert!(d.package_references.is_empty());
    }
}
