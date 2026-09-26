//! Windows Installer (`.msi`) packaging.
//!
//! Vendored and adapted from Deno's `create_windows_msi` and its helpers in
//! `cli/tools/desktop.rs` (MIT, Copyright (c) the Deno authors). The database
//! is authored entirely in pure Rust (`msi` + `cab`), so it cross-compiles from
//! any host — only the target must be Windows.
//!
//! The app installs per-machine under `ProgramFiles64Folder\<App>\`, mirroring
//! the staged app dir; uninstall removes it. A Start-Menu shortcut targets
//! `<App>.exe`, and (beyond Deno) an `Icon` table carries `AppIcon.ico` so the
//! shortcut and Add/Remove Programs show the app icon.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use crate::platform;
use crate::ui;

/// App identity and options for an MSI build.
pub(crate) struct Spec<'a> {
    /// Reverse-DNS identifier (deterministic GUID seed).
    pub identifier: Option<&'a str>,
    /// App version (`major.minor.build`; reduced/warned as needed).
    pub version: Option<&'a str>,
    /// Publisher name shown by Add/Remove Programs.
    pub manufacturer: &'a str,
    /// Optional `.ico` copied into the MSI `Icon` table.
    pub icon: Option<&'a Path>,
}

/// Fixed namespace for deriving deterministic MSI GUIDs (ProductCode,
/// UpgradeCode, package code, component GUIDs) from the app identity via
/// UUIDv5, so an identical app produces an identical installer and upgrades
/// detect earlier versions.
const MSI_GUID_NAMESPACE: uuid::Uuid =
    uuid::Uuid::from_u128(0x6f1d3c8a_4b2e_4f5a_9c7d_8e0f1a2b3c4d);

/// Format a UUID as an MSI registry-format GUID: braced, uppercase, hyphenated.
fn msi_guid(uuid: uuid::Uuid) -> String {
    format!("{{{}}}", uuid.as_hyphenated().to_string().to_uppercase())
}

/// Derive a deterministic GUID for a role (e.g. `product:1.0.0`, `upgrade`).
fn msi_derive_guid(identifier: &str, role: &str) -> String {
    let name = format!("{identifier}\0{role}");
    msi_guid(uuid::Uuid::new_v5(&MSI_GUID_NAMESPACE, name.as_bytes()))
}

/// Map a target triple (or the host arch) to the MSI summary-info architecture.
fn msi_arch_for_target(target: &str) -> Result<&'static str, String> {
    let arch = target.split('-').next().unwrap_or(target);
    match arch {
        "x86_64" => Ok("x64"),
        "aarch64" => Ok("Arm64"),
        other => Err(format!(
            "no MSI architecture mapping for arch '{other}'; supported: x86_64, aarch64"
        )),
    }
}

/// Lowercase base36 encoding of a counter, used to mint unique 8.3 short names.
fn base36(mut n: u32) -> String {
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// Mint a unique DOS 8.3 short name for the MSI `DefaultDir` / `File.FileName`
/// `"short|long"` syntax: `F<base36>` (+ 3-char extension) or `D<base36>`.
fn msi_short_name(counter: u32, long: &str, is_dir: bool) -> String {
    let token = base36(counter).to_uppercase();
    if is_dir {
        return format!("D{token}");
    }
    let ext: String = long
        .rsplit_once('.')
        .map(|(_, e)| e)
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(3)
        .collect::<String>()
        .to_uppercase();
    if ext.is_empty() {
        format!("F{token}")
    } else {
        format!("F{token}.{ext}")
    }
}

/// A staged file destined for both the embedded cabinet and the MSI `File`
/// table.
struct MsiFile {
    key: String,
    component: String,
    file_name: String,
    size: u64,
    abs_path: PathBuf,
}

/// Reduce a configured version to MSI's numeric `major.minor.build` form.
fn msi_product_version(config_version: Option<&str>) -> Result<String, String> {
    let Some(version) = config_version else {
        return Ok("1.0.0".to_string());
    };
    let Some(fields) = numeric_version_fields(version) else {
        return Ok("1.0.0".to_string());
    };
    // Windows Installer packs ProductVersion into 8/8/16 bits.
    const LIMITS: [(u64, &str); 3] = [(255, "major"), (255, "minor"), (65535, "build")];
    for (field, (limit, name)) in fields.iter().zip(LIMITS) {
        if *field > limit {
            return Err(format!(
                "version \"{version}\" cannot be used as an MSI ProductVersion: the {name} \
                 field {field} exceeds the maximum of {limit}. Windows Installer packs \
                 ProductVersion into major(0-255).minor(0-255).build(0-65535); use a version \
                 within those bounds, or build a non-MSI format."
            ));
        }
    }
    Ok(fields
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join("."))
}

/// Warn when a configured version can't be carried into a numeric field.
fn warn_about_unusable_version(config_version: Option<&str>, consequence: &str) {
    let Some(version) = config_version else {
        return;
    };
    if numeric_version_fields(version).is_none() {
        ui::warn(format!("version {version:?} is not numeric; {consequence}"));
    } else if version_core_fields(version).len() > 3 {
        ui::warn(format!(
            "version {version:?} has more than three numeric fields; only the leading \
             major.minor.build are used"
        ));
    }
}

/// The leading numeric `major[.minor[.build]]` fields of a version, dropping a
/// semver prerelease/build suffix. `None` when not numeric at all.
fn numeric_version_fields(version: &str) -> Option<Vec<u64>> {
    version_core_fields(version)
        .iter()
        .take(3)
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()
}

/// The dot-separated fields of a version's numeric core.
fn version_core_fields(version: &str) -> Vec<&str> {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    core.split('.').collect()
}

/// Wrap a Windows app directory in a Windows Installer `.msi` package.
pub(crate) fn create(app_dir: &Path, msi_path: &Path, spec: &Spec) -> Result<(), String> {
    use msi::CodePage;
    use msi::Column;
    use msi::Insert;
    use msi::Package;
    use msi::PackageType;
    use msi::Value;

    let app_name = app_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "App".to_string());
    warn_about_unusable_version(
        spec.version,
        "the .msi will be built with ProductVersion 1.0.0 (MSI accepts only \
         major.minor.build)",
    );
    let version = msi_product_version(spec.version)?;
    let version = version.as_str();
    let identifier = spec
        .identifier
        .map(str::to_string)
        .unwrap_or_else(|| format!("com.inka.desktop.{}", app_name.to_lowercase()));
    let manufacturer = spec.manufacturer;
    let arch = msi_arch_for_target(platform::TARGET)?;
    // An optional icon, read once; both the `Icon` table stream and (leave a
    // gap for) the shortcut reference use it.
    let icon_bytes = match spec.icon {
        Some(path) => Some(
            std::fs::read(path).map_err(|e| format!("cannot read icon {}: {e}", path.display()))?,
        ),
        None => None,
    };

    // --- Walk the staged tree: register every directory, then every file. ----
    let mut rel_files: Vec<(PathBuf, u64)> = Vec::new();
    let mut rel_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut stack = vec![app_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let mut entries: Vec<_> = entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let md = std::fs::symlink_metadata(&path)
                .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
            if md.is_dir() {
                rel_dirs.insert(
                    path.strip_prefix(app_dir)
                        .map_err(|_| format!("path {} outside app dir", path.display()))?
                        .to_path_buf(),
                );
                stack.push(path);
            } else if md.is_file() {
                rel_files.push((
                    path.strip_prefix(app_dir)
                        .map_err(|_| format!("path {} outside app dir", path.display()))?
                        .to_path_buf(),
                    md.len(),
                ));
            }
        }
    }
    rel_files.sort();

    // Assign a Directory id to every directory. Root → INSTALLDIR; nested →
    // d0, d1, … in sorted order.
    let mut dir_ids: BTreeMap<PathBuf, String> = BTreeMap::new();
    dir_ids.insert(PathBuf::new(), "INSTALLDIR".to_string());
    for (i, dir) in rel_dirs.iter().enumerate() {
        dir_ids.insert(dir.clone(), format!("d{i}"));
    }

    let pf_folder = "ProgramFiles64Folder";

    // --- Directory table rows. ----------------------------------------------
    let mut short_counter: u32 = 0;
    let mut directory_rows: Vec<Vec<Value>> = vec![
        vec![
            Value::Str("TARGETDIR".to_string()),
            Value::Null,
            Value::Str("SourceDir".to_string()),
        ],
        vec![
            Value::Str(pf_folder.to_string()),
            Value::Str("TARGETDIR".to_string()),
            Value::Str(".".to_string()),
        ],
        vec![
            Value::Str("INSTALLDIR".to_string()),
            Value::Str(pf_folder.to_string()),
            Value::Str(format!(
                "{}|{}",
                msi_short_name(
                    {
                        short_counter += 1;
                        short_counter
                    },
                    &app_name,
                    true
                ),
                app_name
            )),
        ],
    ];
    for dir in &rel_dirs {
        let id = dir_ids[dir].clone();
        let parent = dir_ids[dir.parent().unwrap_or(Path::new(""))].clone();
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| id.clone());
        short_counter += 1;
        directory_rows.push(vec![
            Value::Str(id),
            Value::Str(parent),
            Value::Str(format!(
                "{}|{}",
                msi_short_name(short_counter, &name, true),
                name
            )),
        ]);
    }

    // --- Components: one per directory that directly contains files. ---------
    let mut comp_for_dir: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut files: Vec<MsiFile> = Vec::new();
    for (rel, size) in &rel_files {
        let dir = rel.parent().unwrap_or(Path::new("")).to_path_buf();
        let next_comp = format!("c{}", comp_for_dir.len());
        let component = comp_for_dir.entry(dir.clone()).or_insert(next_comp).clone();
        let key = format!("f{}", files.len());
        let long = rel
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| key.clone());
        short_counter += 1;
        files.push(MsiFile {
            key,
            component,
            file_name: format!("{}|{}", msi_short_name(short_counter, &long, false), long),
            size: *size,
            abs_path: app_dir.join(rel),
        });
    }
    if files.is_empty() {
        return Err("cannot build a .msi from an empty app directory".to_string());
    }

    // Locate the launcher in the install root for a Start-Menu shortcut. It is
    // the backend binary renamed `<app>.exe`, auto-loading the co-located
    // `<app>.dll`, so the shortcut targets it directly.
    let launcher_exe = format!("{app_name}.exe").to_ascii_lowercase();
    let shortcut_target = rel_files
        .iter()
        .zip(files.iter())
        .find(|((rel, _), _)| {
            rel.parent() == Some(Path::new(""))
                && rel
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.to_ascii_lowercase() == launcher_exe)
        })
        .map(|(_, f)| (f.key.clone(), f.component.clone()));

    if shortcut_target.is_some() {
        directory_rows.push(vec![
            Value::Str("ProgramMenuFolder".to_string()),
            Value::Str("TARGETDIR".to_string()),
            Value::Str(".".to_string()),
        ]);
    }

    // msidbComponentAttributes64bit (256).
    const COMPONENT_64BIT: i32 = 256;
    let component_rows: Vec<Vec<Value>> = comp_for_dir
        .iter()
        .map(|(dir, comp)| {
            let dir_id = dir_ids[dir].clone();
            let keypath = files
                .iter()
                .find(|f| &f.component == comp)
                .map(|f| f.key.clone())
                .unwrap();
            vec![
                Value::Str(comp.clone()),
                Value::Str(msi_derive_guid(&identifier, &format!("component:{comp}"))),
                Value::Str(dir_id),
                Value::Int(COMPONENT_64BIT),
                Value::Null,
                Value::Str(keypath),
            ]
        })
        .collect();

    // --- File table + cabinet payload (shared 1-based sequence). -------------
    // msidbFileAttributesVital (512).
    const FILE_VITAL: i32 = 512;
    let file_rows: Vec<Vec<Value>> = files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            vec![
                Value::Str(f.key.clone()),
                Value::Str(f.component.clone()),
                Value::Str(f.file_name.clone()),
                Value::Int(f.size as i32),
                Value::Null,
                Value::Null,
                Value::Int(FILE_VITAL),
                Value::Int(1 + i as i32),
            ]
        })
        .collect();

    let cab_bytes = build_msi_cabinet(&files)?;

    // --- Author the MSI database. -------------------------------------------
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    let mut package = Package::create(PackageType::Installer, &mut cursor)
        .map_err(|e| format!("cannot create .msi database: {e}"))?;

    // Windows Installer rejects a UTF-8 database; use Windows-1252 (all our
    // strings are ASCII, so the encoding is identical).
    package.set_database_codepage(CodePage::Windows1252);

    {
        let summary = package.summary_info_mut();
        summary.set_codepage(CodePage::Windows1252);
        summary.set_title(format!("{app_name} Installer"));
        summary.set_subject(app_name.clone());
        summary.set_author(manufacturer.to_string());
        summary.set_comments(format!("{app_name} desktop application"));
        summary.set_arch(arch);
        summary.set_languages(&[msi::Language::from_code(1033)]);
        summary.set_creating_application("inka desktop");
        summary.set_uuid(uuid::Uuid::new_v5(
            &MSI_GUID_NAMESPACE,
            format!("{identifier}\0package:{version}").as_bytes(),
        ));
        // Source compressed; long file names allowed.
        summary.set_word_count(2);
        // Minimum Windows Installer version (2.00).
        summary.set_page_count(200);
        // Fixed creation time (2020-01-01) for reproducible output.
        summary.set_creation_time(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_577_836_800),
        );
    }

    // Table schemas.
    package
        .create_table(
            "Directory",
            vec![
                Column::build("Directory").primary_key().id_string(72),
                Column::build("Directory_Parent").nullable().id_string(72),
                Column::build("DefaultDir")
                    .category(msi::Category::DefaultDir)
                    .string(255),
            ],
        )
        .map_err(|e| format!("cannot create Directory table: {e}"))?;
    package
        .create_table(
            "Component",
            vec![
                Column::build("Component").primary_key().id_string(72),
                Column::build("ComponentId")
                    .nullable()
                    .category(msi::Category::Guid)
                    .string(38),
                Column::build("Directory_").id_string(72),
                Column::build("Attributes").int16(),
                Column::build("Condition")
                    .nullable()
                    .category(msi::Category::Condition)
                    .string(255),
                Column::build("KeyPath").nullable().id_string(72),
            ],
        )
        .map_err(|e| format!("cannot create Component table: {e}"))?;
    package
        .create_table(
            "Feature",
            vec![
                Column::build("Feature").primary_key().id_string(38),
                Column::build("Feature_Parent").nullable().id_string(38),
                Column::build("Title").nullable().text_string(64),
                Column::build("Description").nullable().text_string(255),
                Column::build("Display").nullable().int16(),
                Column::build("Level").int16(),
                Column::build("Directory_").nullable().id_string(72),
                Column::build("Attributes").int16(),
            ],
        )
        .map_err(|e| format!("cannot create Feature table: {e}"))?;
    package
        .create_table(
            "FeatureComponents",
            vec![
                Column::build("Feature_").primary_key().id_string(38),
                Column::build("Component_").primary_key().id_string(72),
            ],
        )
        .map_err(|e| format!("cannot create FeatureComponents table: {e}"))?;
    package
        .create_table(
            "File",
            vec![
                Column::build("File").primary_key().id_string(72),
                Column::build("Component_").id_string(72),
                Column::build("FileName")
                    .category(msi::Category::Filename)
                    .string(255),
                Column::build("FileSize").int32(),
                Column::build("Version")
                    .nullable()
                    .category(msi::Category::Version)
                    .string(72),
                Column::build("Language").nullable().string(20),
                Column::build("Attributes").nullable().int16(),
                Column::build("Sequence").int16(),
            ],
        )
        .map_err(|e| format!("cannot create File table: {e}"))?;
    package
        .create_table(
            "Media",
            vec![
                Column::build("DiskId").primary_key().int16(),
                Column::build("LastSequence").int16(),
                Column::build("DiskPrompt").nullable().text_string(64),
                Column::build("Cabinet")
                    .nullable()
                    .category(msi::Category::Cabinet)
                    .string(255),
                Column::build("VolumeLabel").nullable().text_string(32),
                Column::build("Source")
                    .nullable()
                    .category(msi::Category::Property)
                    .string(72),
            ],
        )
        .map_err(|e| format!("cannot create Media table: {e}"))?;
    package
        .create_table(
            "Property",
            vec![
                Column::build("Property").primary_key().id_string(72),
                Column::build("Value").text_string(0),
            ],
        )
        .map_err(|e| format!("cannot create Property table: {e}"))?;
    if shortcut_target.is_some() {
        package
            .create_table(
                "Shortcut",
                vec![
                    Column::build("Shortcut").primary_key().id_string(72),
                    Column::build("Directory_").id_string(72),
                    Column::build("Name")
                        .category(msi::Category::Filename)
                        .string(128),
                    Column::build("Component_").id_string(72),
                    Column::build("Target")
                        .category(msi::Category::Shortcut)
                        .string(72),
                    Column::build("Arguments")
                        .nullable()
                        .category(msi::Category::Formatted)
                        .string(255),
                    Column::build("Description").nullable().text_string(255),
                    Column::build("Hotkey").nullable().int16(),
                    Column::build("Icon_").nullable().id_string(72),
                    Column::build("IconIndex").nullable().int16(),
                    Column::build("ShowCmd").nullable().int16(),
                    Column::build("WkDir").nullable().id_string(72),
                ],
            )
            .map_err(|e| format!("cannot create Shortcut table: {e}"))?;
    }
    // The optional icon table (`Name` primary key, `Data` object stream). The
    // stream is written below under the same name.
    if icon_bytes.is_some() {
        package
            .create_table(
                "Icon",
                vec![
                    Column::build("Name").primary_key().id_string(72),
                    Column::build("Data").binary(),
                ],
            )
            .map_err(|e| format!("cannot create Icon table: {e}"))?;
    }
    // `AdminExecuteSequence` drives `msiexec /a` (administrative install /
    // extraction); without it `/a` runs no actions and extracts nothing.
    for table in [
        "InstallExecuteSequence",
        "InstallUISequence",
        "AdminExecuteSequence",
    ] {
        package
            .create_table(
                table,
                vec![
                    Column::build("Action").primary_key().id_string(72),
                    Column::build("Condition")
                        .nullable()
                        .category(msi::Category::Condition)
                        .string(255),
                    Column::build("Sequence").nullable().int16(),
                ],
            )
            .map_err(|e| format!("cannot create {table} table: {e}"))?;
    }

    // --- Populate tables. ----------------------------------------------------
    package
        .insert_rows(Insert::into("Directory").rows(directory_rows))
        .map_err(|e| format!("cannot insert Directory rows: {e}"))?;
    package
        .insert_rows(Insert::into("Component").rows(component_rows))
        .map_err(|e| format!("cannot insert Component rows: {e}"))?;
    package
        .insert_rows(Insert::into("Feature").row(vec![
            Value::Str("MainFeature".to_string()),
            Value::Null,
            Value::Str(app_name.clone()),
            Value::Null,
            Value::Int(1),
            Value::Int(1),
            Value::Str("INSTALLDIR".to_string()),
            Value::Int(0),
        ]))
        .map_err(|e| format!("cannot insert Feature row: {e}"))?;
    package
        .insert_rows(
            Insert::into("FeatureComponents").rows(
                comp_for_dir
                    .values()
                    .map(|c| vec![Value::Str("MainFeature".to_string()), Value::Str(c.clone())])
                    .collect(),
            ),
        )
        .map_err(|e| format!("cannot insert FeatureComponents rows: {e}"))?;
    package
        .insert_rows(Insert::into("File").rows(file_rows))
        .map_err(|e| format!("cannot insert File rows: {e}"))?;
    package
        .insert_rows(Insert::into("Media").row(vec![
            Value::Int(1),
            Value::Int(files.len() as i32),
            Value::Null,
            Value::Str("#appcab".to_string()),
            Value::Null,
            Value::Null,
        ]))
        .map_err(|e| format!("cannot insert Media row: {e}"))?;
    if icon_bytes.is_some() {
        package
            .insert_rows(
                Insert::into("Icon").row(vec![Value::Str("AppIcon".to_string()), Value::Binary]),
            )
            .map_err(|e| format!("cannot insert Icon row: {e}"))?;
    }
    if let Some((launcher_key, launcher_comp)) = &shortcut_target {
        short_counter += 1;
        let short_name = msi_short_name(short_counter, &app_name, false);
        package
            .insert_rows(Insert::into("Shortcut").row(vec![
                Value::Str("AppShortcut".to_string()),
                Value::Str("ProgramMenuFolder".to_string()),
                Value::Str(format!("{short_name}|{app_name}")),
                Value::Str(launcher_comp.clone()),
                // Non-advertised shortcut: `[#key]` resolves to the installed exe.
                Value::Str(format!("[#{launcher_key}]")),
                Value::Null, // Arguments (co-located auto-load)
                Value::Null, // Description
                Value::Null, // Hotkey
                if icon_bytes.is_some() {
                    Value::Str("AppIcon".to_string())
                } else {
                    Value::Null
                },
                if icon_bytes.is_some() {
                    Value::Int(0)
                } else {
                    Value::Null
                },
                Value::Null,                          // ShowCmd
                Value::Str("INSTALLDIR".to_string()), // WkDir
            ]))
            .map_err(|e| format!("cannot insert Shortcut row: {e}"))?;
    }

    let product_code = msi_derive_guid(&identifier, &format!("product:{version}"));
    let upgrade_code = msi_derive_guid(&identifier, "upgrade");
    let mut property_rows: Vec<Vec<Value>> = vec![
        vec![
            Value::Str("ProductCode".to_string()),
            Value::Str(product_code),
        ],
        vec![
            Value::Str("ProductName".to_string()),
            Value::Str(app_name.clone()),
        ],
        vec![
            Value::Str("ProductVersion".to_string()),
            Value::Str(version.to_string()),
        ],
        vec![
            Value::Str("ProductLanguage".to_string()),
            Value::Str("1033".to_string()),
        ],
        vec![
            Value::Str("Manufacturer".to_string()),
            Value::Str(manufacturer.to_string()),
        ],
        vec![
            Value::Str("UpgradeCode".to_string()),
            Value::Str(upgrade_code),
        ],
        // Per-machine install (into Program Files).
        vec![
            Value::Str("ALLUSERS".to_string()),
            Value::Str("1".to_string()),
        ],
    ];
    if icon_bytes.is_some() {
        property_rows.push(vec![
            Value::Str("ARPPRODUCTICON".to_string()),
            Value::Str("AppIcon".to_string()),
        ]);
    }
    package
        .insert_rows(Insert::into("Property").rows(property_rows))
        .map_err(|e| format!("cannot insert Property rows: {e}"))?;

    // Standard action sequences for a basic per-machine install + uninstall.
    let mut exec_seq: Vec<(&str, i32)> = vec![
        ("CostInitialize", 800),
        ("FileCost", 900),
        ("CostFinalize", 1000),
        ("InstallValidate", 1400),
        ("InstallInitialize", 1500),
        ("ProcessComponents", 1600),
        ("UnpublishFeatures", 1800),
        ("RemoveFiles", 3500),
        ("InstallFiles", 4000),
        ("RegisterProduct", 6100),
        ("PublishFeatures", 6300),
        ("PublishProduct", 6400),
        ("InstallFinalize", 6600),
    ];
    if shortcut_target.is_some() {
        exec_seq.push(("RemoveShortcuts", 3800));
        exec_seq.push(("CreateShortcuts", 4500));
    }
    package
        .insert_rows(
            Insert::into("InstallExecuteSequence").rows(
                exec_seq
                    .iter()
                    .map(|(a, s)| vec![Value::Str(a.to_string()), Value::Null, Value::Int(*s)])
                    .collect(),
            ),
        )
        .map_err(|e| format!("cannot insert InstallExecuteSequence rows: {e}"))?;
    let ui_seq: &[(&str, i32)] = &[
        ("CostInitialize", 800),
        ("FileCost", 900),
        ("CostFinalize", 1000),
        ("ExecuteAction", 1300),
    ];
    package
        .insert_rows(
            Insert::into("InstallUISequence").rows(
                ui_seq
                    .iter()
                    .map(|(a, s)| vec![Value::Str(a.to_string()), Value::Null, Value::Int(*s)])
                    .collect(),
            ),
        )
        .map_err(|e| format!("cannot insert InstallUISequence rows: {e}"))?;

    // Administrative install (`msiexec /a`): the standard admin sequence, which
    // stands alone and so re-includes the initialization actions. `InstallFiles`
    // extracts the embedded cabinet into `TARGETDIR`.
    let admin_seq: &[(&str, i32)] = &[
        ("CostInitialize", 800),
        ("FileCost", 900),
        ("CostFinalize", 1000),
        ("InstallValidate", 1400),
        ("InstallInitialize", 1500),
        ("InstallAdminPackage", 3900),
        ("InstallFiles", 4000),
        ("InstallFinalize", 6600),
    ];
    package
        .insert_rows(
            Insert::into("AdminExecuteSequence").rows(
                admin_seq
                    .iter()
                    .map(|(a, s)| vec![Value::Str(a.to_string()), Value::Null, Value::Int(*s)])
                    .collect(),
            ),
        )
        .map_err(|e| format!("cannot insert AdminExecuteSequence rows: {e}"))?;

    // Embedded cabinet stream (Media.Cabinet = "#appcab").
    {
        let mut stream = package
            .write_stream("appcab")
            .map_err(|e| format!("cannot write cabinet stream: {e}"))?;
        stream
            .write_all(&cab_bytes)
            .map_err(|e| format!("cannot write cabinet stream: {e}"))?;
    }
    // Optional icon stream, named after the Icon row.
    if let Some(bytes) = &icon_bytes {
        let mut stream = package
            .write_stream("AppIcon")
            .map_err(|e| format!("cannot write icon stream: {e}"))?;
        stream
            .write_all(bytes)
            .map_err(|e| format!("cannot write icon stream: {e}"))?;
    }

    package
        .flush()
        .map_err(|e| format!("cannot finalize .msi database: {e}"))?;
    drop(package);

    if let Some(parent) = msi_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(msi_path, cursor.into_inner())
        .map_err(|e| format!("cannot write {}: {e}", msi_path.display()))?;

    // See `msi_sort_tables_by_string_id`: the `msi` crate's row order is not
    // the string-pool-id order Windows Installer requires, and real `msiexec`
    // rejects an unsorted database with error 2219.
    msi_sort_tables_by_string_id(msi_path)
        .map_err(|e| format!("cannot finalize {}: {e}", msi_path.display()))?;
    Ok(())
}

/// Decode a Windows Installer stream name back to its logical table name.
fn msi_demangle_stream_name(name: &str) -> String {
    const B64: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz._";
    let mut out = String::new();
    for c in name.chars() {
        let v = c as u32;
        if (0x3800..0x4800).contains(&v) {
            let n = v - 0x3800;
            out.push(B64[(n & 0x3f) as usize] as char);
            out.push(B64[((n >> 6) & 0x3f) as usize] as char);
        } else if (0x4800..0x4840).contains(&v) {
            let n = v - 0x4800;
            out.push(B64[(n & 0x3f) as usize] as char);
        } else if v == 0x4840 {
            out.push('\0');
        } else {
            out.push(c);
        }
    }
    out
}

/// Re-sort the rows of a column-major MSI table stream by key columns, which are
/// stored as little-endian unsigned integers (string ids for string columns).
fn msi_resort_stream(data: &[u8], widths: &[usize], keys: &[usize]) -> Vec<u8> {
    let row_width: usize = widths.iter().sum();
    if row_width == 0 {
        return data.to_vec();
    }
    let rows = data.len() / row_width;
    if rows <= 1 {
        return data.to_vec();
    }
    let mut col_off = vec![0usize; widths.len()];
    let mut acc = 0;
    for (c, &w) in widths.iter().enumerate() {
        col_off[c] = acc;
        acc += rows * w;
    }
    let read = |row: usize, col: usize| -> u64 {
        let base = col_off[col] + row * widths[col];
        let mut v = 0u64;
        for k in 0..widths[col] {
            v |= (data[base + k] as u64) << (8 * k);
        }
        v
    };
    let mut order: Vec<usize> = (0..rows).collect();
    order.sort_by(|&a, &b| {
        for &k in keys {
            match read(a, k).cmp(&read(b, k)) {
                std::cmp::Ordering::Equal => continue,
                ord => return ord,
            }
        }
        a.cmp(&b)
    });
    let mut out = vec![0u8; data.len()];
    for (c, &w) in widths.iter().enumerate() {
        for (new_row, &old_row) in order.iter().enumerate() {
            let src = col_off[c] + old_row * w;
            let dst = col_off[c] + new_row * w;
            out[dst..dst + w].copy_from_slice(&data[src..src + w]);
        }
    }
    out
}

/// Sort every persistent table in a freshly written `.msi` by primary key in
/// string-pool-id order, as Windows Installer requires (error 2219 otherwise).
fn msi_sort_tables_by_string_id(msi_path: &Path) -> Result<(), String> {
    let mut comp = cfb::open_rw(msi_path).map_err(|e| format!("cannot open .msi: {e}"))?;
    let names: Vec<String> = comp
        .read_storage("/")
        .map_err(|e| format!("cannot read .msi root storage: {e}"))?
        .map(|e| e.name().to_string())
        .collect();
    let read_stream =
        |comp: &mut cfb::CompoundFile<std::fs::File>, raw: &str| -> Result<Vec<u8>, String> {
            let mut s = comp
                .open_stream(format!("/{raw}"))
                .map_err(|e| format!("cannot open stream {raw}: {e}"))?;
            let mut b = Vec::new();
            s.read_to_end(&mut b)
                .map_err(|e| format!("cannot read stream {raw}: {e}"))?;
            Ok(b)
        };
    let write_stream = |comp: &mut cfb::CompoundFile<std::fs::File>,
                        raw: &str,
                        bytes: &[u8]|
     -> Result<(), String> {
        let mut s = comp
            .open_stream(format!("/{raw}"))
            .map_err(|e| format!("cannot open stream {raw}: {e}"))?;
        s.write_all(bytes)
            .map_err(|e| format!("cannot write stream {raw}: {e}"))?;
        Ok(())
    };
    let find_raw = |suffix: &str| -> Option<String> {
        names
            .iter()
            .find(|n| msi_demangle_stream_name(n).ends_with(suffix))
            .cloned()
    };

    // The string-pool header's top bit selects 3-byte string references.
    let pool_raw = find_raw("_StringPool")
        .ok_or_else(|| "malformed .msi: no _StringPool stream".to_string())?;
    let pool = read_stream(&mut comp, &pool_raw)?;
    let str_w: usize = if pool.len() >= 4 && (pool[3] & 0x80) != 0 {
        3
    } else {
        2
    };

    let data_raw = find_raw("_StringData")
        .ok_or_else(|| "malformed .msi: no _StringData stream".to_string())?;
    let str_data = read_stream(&mut comp, &data_raw)?;
    let mut strings = vec![String::new()]; // 1-based; index 0 unused.
    {
        let mut off = 0usize;
        let mut i = 4;
        while i + 4 <= pool.len() {
            let len = u16::from_le_bytes([pool[i], pool[i + 1]]) as usize;
            let end = (off + len).min(str_data.len());
            strings.push(String::from_utf8_lossy(&str_data[off..end]).into_owned());
            off += len;
            i += 4;
        }
    }
    let id_of =
        |text: &str| -> Option<u64> { strings.iter().position(|s| s == text).map(|i| i as u64) };

    // System tables have a fixed schema not described in `_Columns`.
    let system_tables: &[(&str, Vec<usize>, Vec<usize>)] = &[
        ("_Tables", vec![str_w], vec![0]),
        ("_Columns", vec![str_w, 2, str_w, 2], vec![0, 1]),
        (
            "_Validation",
            vec![str_w, str_w, str_w, 4, 4, str_w, 2, str_w, str_w, str_w],
            vec![0, 1],
        ),
    ];
    for (name, widths, keys) in system_tables {
        if let Some(raw) = find_raw(name) {
            let bytes = read_stream(&mut comp, &raw)?;
            let sorted = msi_resort_stream(&bytes, widths, keys);
            write_stream(&mut comp, &raw, &sorted)?;
        }
    }

    // Parse the (now sorted) `_Columns` table for each persistent table's
    // column widths and primary keys.
    let columns_raw =
        find_raw("_Columns").ok_or_else(|| "malformed .msi: no _Columns stream".to_string())?;
    let columns = read_stream(&mut comp, &columns_raw)?;
    let col_row_w = str_w + 2 + str_w + 2; // Table, Number, Name, Type
    let ncol = columns.len() / col_row_w;
    let read_col = |arr_off: usize, row: usize, w: usize| -> u64 {
        let base = arr_off + row * w;
        let mut v = 0u64;
        for k in 0..w {
            v |= (columns[base + k] as u64) << (8 * k);
        }
        v
    };
    let off_table = 0usize;
    let off_number = ncol * str_w;
    let off_type = off_number + ncol * 2 + ncol * str_w;
    // Type word: 0x0800 = string, 0x2000 = primary key; low byte = int width.
    let width_of = |ty: u64| -> usize {
        let t = (ty ^ 0x8000) & 0xffff;
        if (t & 0x0800) != 0 {
            str_w
        } else if (t & 0xff) == 4 {
            4
        } else {
            2
        }
    };
    let is_key = |ty: u64| -> bool { ((ty ^ 0x8000) & 0x2000) != 0 };
    let mut table_columns: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
    for r in 0..ncol {
        let table_id = read_col(off_table, r, str_w);
        let number = read_col(off_number, r, 2) ^ 0x8000;
        let ty = read_col(off_type, r, 2);
        table_columns
            .entry(table_id)
            .or_default()
            .push((number, ty));
    }
    for cols in table_columns.values_mut() {
        cols.sort_by_key(|&(number, _)| number);
    }

    for raw in &names {
        let demangled = msi_demangle_stream_name(raw);
        let Some(table_name) = demangled.strip_prefix('\0') else {
            continue;
        };
        if table_name.starts_with('_') {
            continue;
        }
        let Some(table_id) = id_of(table_name) else {
            continue;
        };
        let Some(cols) = table_columns.get(&table_id) else {
            continue;
        };
        let widths: Vec<usize> = cols.iter().map(|&(_, ty)| width_of(ty)).collect();
        let keys: Vec<usize> = cols
            .iter()
            .enumerate()
            .filter(|&(_, &(_, ty))| is_key(ty))
            .map(|(i, _)| i)
            .collect();
        if keys.is_empty() {
            continue;
        }
        let bytes = read_stream(&mut comp, raw)?;
        let sorted = msi_resort_stream(&bytes, &widths, &keys);
        write_stream(&mut comp, raw, &sorted)?;
    }

    comp.flush()
        .map_err(|e| format!("cannot flush .msi: {e}"))?;
    Ok(())
}

/// Build the embedded MSZIP cabinet carrying every install file, named by its
/// MSI `File` key, in sequence order.
fn build_msi_cabinet(files: &[MsiFile]) -> Result<Vec<u8>, String> {
    let mut builder = cab::CabinetBuilder::new();
    {
        let folder = builder.add_folder(cab::CompressionType::MsZip);
        for f in files {
            folder.add_file(f.key.clone());
        }
    }

    let cursor = std::io::Cursor::new(Vec::<u8>::new());
    let mut writer = builder
        .build(cursor)
        .map_err(|e| format!("cannot start cabinet: {e}"))?;
    let by_key: HashMap<&str, &MsiFile> = files.iter().map(|f| (f.key.as_str(), f)).collect();
    while let Some(mut file_writer) = writer
        .next_file()
        .map_err(|e| format!("cannot advance cabinet writer: {e}"))?
    {
        let name = file_writer.file_name().to_string();
        let f = by_key
            .get(name.as_str())
            .ok_or_else(|| format!("cabinet file {name} missing"))?;
        let data = std::fs::read(&f.abs_path)
            .map_err(|e| format!("cannot read {}: {e}", f.abs_path.display()))?;
        file_writer
            .write_all(&data)
            .map_err(|e| format!("cannot write cabinet file {name}: {e}"))?;
    }
    let cursor = writer
        .finish()
        .map_err(|e| format!("cannot finish cabinet: {e}"))?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "inka-msi-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_app(dir: &Path) {
        std::fs::create_dir_all(dir.join("locales")).unwrap();
        std::fs::write(dir.join("Mail.exe"), b"exe").unwrap();
        std::fs::write(dir.join("Mail.dll"), b"dll").unwrap();
        std::fs::write(dir.join("runtime-version"), b"0.1.0\n").unwrap();
        std::fs::write(dir.join("AppIcon.ico"), b"ico-bytes").unwrap();
        std::fs::write(dir.join("locales/en-US.pak"), b"pak").unwrap();
    }

    #[test]
    fn builds_and_roundtrips_an_msi() {
        let base = scratch("build");
        let app = base.join("Mail");
        fake_app(&app);
        let msi_path = base.join("Mail.msi");
        let icon = app.join("AppIcon.ico");
        let spec = Spec {
            identifier: Some("com.acme.mail"),
            version: Some("1.4.0"),
            manufacturer: "Acme",
            icon: Some(&icon),
        };
        create(&app, &msi_path, &spec).unwrap();

        // A compound document starts with the CFB magic.
        let bytes = std::fs::read(&msi_path).unwrap();
        assert_eq!(
            &bytes[..8],
            &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]
        );

        let f = std::fs::File::open(&msi_path).unwrap();
        let mut pkg = msi::Package::open(f).unwrap();
        for table in [
            "Directory",
            "Component",
            "Feature",
            "FeatureComponents",
            "File",
            "Media",
            "Property",
            "Shortcut",
            "Icon",
            "InstallExecuteSequence",
            "AdminExecuteSequence",
        ] {
            assert!(pkg.has_table(table), "missing table {table}");
        }
        // `msiexec /a` needs `InstallFiles` in the standalone admin sequence.
        let admin: Vec<String> = pkg
            .select_rows(msi::Select::table("AdminExecuteSequence"))
            .unwrap()
            .map(|r| r[0].as_str().unwrap().to_string())
            .collect();
        for action in [
            "CostInitialize",
            "CostFinalize",
            "InstallAdminPackage",
            "InstallFiles",
            "InstallFinalize",
        ] {
            assert!(
                admin.iter().any(|a| a == action),
                "admin missing {action}: {admin:?}"
            );
        }
        let file_rows = pkg.select_rows(msi::Select::table("File")).unwrap().len();
        assert_eq!(file_rows, 5, "exe, dll, runtime-version, ico, locale");
        let props = pkg.select_rows(msi::Select::table("Property")).unwrap();
        let name = props.into_iter().find_map(|r| {
            (r[0].as_str() == Some("ProductName")).then(|| r[1].as_str().unwrap().to_string())
        });
        assert_eq!(name.as_deref(), Some("Mail"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn msi_short_name_and_version_rules() {
        assert_eq!(msi_short_name(42, "libcef.dll", false), "F16.DLL");
        assert_eq!(msi_short_name(2, "LICENSE", false), "F2");
        assert_eq!(msi_short_name(1, "locales", true), "D1");
        assert_eq!(msi_product_version(Some("01.02.03")).unwrap(), "1.2.3");
        assert_eq!(msi_product_version(Some("2.3")).unwrap(), "2.3");
        assert_eq!(msi_product_version(Some("2.3.4-beta.1")).unwrap(), "2.3.4");
        assert_eq!(msi_product_version(Some("1.2.3.4")).unwrap(), "1.2.3");
        assert_eq!(msi_product_version(Some("weird")).unwrap(), "1.0.0");
        assert_eq!(msi_product_version(None).unwrap(), "1.0.0");
        assert!(msi_product_version(Some("2026.8.26")).is_err());
        assert_eq!(
            msi_arch_for_target("x86_64-pc-windows-msvc").unwrap(),
            "x64"
        );
        assert_eq!(
            msi_arch_for_target("aarch64-apple-darwin").unwrap(),
            "Arm64"
        );
        assert!(msi_arch_for_target("i686-pc-windows-msvc").is_err());
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
    }

    #[test]
    fn msi_demangle_roundtrips_table_stream_names() {
        // The sentinel 0x4840 decodes to a leading NUL for table-data streams.
        let mut encoded = String::new();
        encoded.push('\u{4840}');
        // A two-digit group encodes char0 in the low 6 bits, char1 in the high
        // 6 bits; B64 indices: 'a' = 36, '0' = 0.
        let first = 36u32; // 'a'
        let second = 0u32; // '0'
        encoded.push(char::from_u32(0x3800 + (first | (second << 6))).unwrap());
        assert_eq!(msi_demangle_stream_name(&encoded), "\0a0");
    }

    #[test]
    fn empty_app_dir_errors() {
        let base = scratch("empty");
        let app = base.join("Empty");
        std::fs::create_dir_all(&app).unwrap();
        let spec = Spec {
            identifier: None,
            version: None,
            manufacturer: "x",
            icon: None,
        };
        assert!(create(&app, &base.join("Empty.msi"), &spec).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
