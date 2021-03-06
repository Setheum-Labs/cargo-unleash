use crate::cli::GenerateReadmeMode;
use crate::commands;
use cargo::core::{Manifest, Package, Workspace};
use lazy_static::lazy_static;
use regex::{Captures, Regex};
use sha1::Sha1;
use std::{
    error::Error,
    fmt::Display,
    fs::{self, File},
    path::{Path, PathBuf},
};
use toml_edit::Value;

static DEFAULT_DOC_URI: &str = "https://docs.rs/";

lazy_static! {
    // See http://blog.michaelperrin.fr/2019/02/04/advanced-regular-expressions/
    static ref RELATIVE_LINKS_REGEX: Regex = 
        Regex::new(r#"\[(?P<text>.+)\]\((?P<url>[^ ]+)(?: "(?P<title>.+)")?\)"#).unwrap();
}

#[derive(Debug)]
pub enum CheckReadmeResult {
    Skipped,
    Missing,
    UpdateNeeded,
    UpToDate,
}

impl Display for CheckReadmeResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Skipped => "Skipped",
                Self::Missing => "Missing",
                Self::UpdateNeeded => "Update needed",
                Self::UpToDate => "Up-to-date",
            }
        )
    }
}

pub fn check_pkg_readme<'a>(
    ws: &Workspace<'a>,
    pkg_path: &Path,
    pkg_manifest: &Manifest,
) -> Result<(), String> {
    let c = ws.config();

    let mut pkg_source = find_entrypoint(pkg_path)?;
    let readme_path = pkg_path.join("README.md");

    c.shell()
        .status("Checking", format!("Readme for {}", &pkg_manifest.name()))
        .map_err(|e| format!("{:}", e))?;

    let pkg_readme = fs::read_to_string(readme_path.clone());
    match pkg_readme {
        Ok(pkg_readme) => {
            // Try to find readme template
            let template_path = find_readme_template(&ws.root(), &pkg_path)?;

            let new_readme = generate_readme(&pkg_path, &mut pkg_source, template_path)?;
            if Sha1::from(pkg_readme) == Sha1::from(new_readme) {
                Ok(())
            } else {
                Err(CheckReadmeResult::UpdateNeeded.to_string())
            }
        }
        Err(_err) => Err(CheckReadmeResult::Missing.to_string()),
    }
}

pub fn gen_all_readme<'a>(
    packages: Vec<Package>,
    ws: &Workspace<'a>,
    readme_mode: GenerateReadmeMode,
) -> Result<(), Box<dyn Error>> {
    let c = ws.config();
    c.shell().status("Generating", "Readme files")?;
    for pkg in packages.into_iter() {
        let pkg_name = &pkg.name().clone();
        gen_pkg_readme(ws, pkg, &readme_mode)
            .map_err(|e| format!("Failure generating Readme for {:}: {}", pkg_name, e))?
    }

    Ok(())
}

pub fn gen_pkg_readme<'a>(
    ws: &Workspace<'a>,
    pkg: Package,
    mode: &GenerateReadmeMode,
) -> Result<(), String> {
    let c = ws.config();
    let root_path = ws.root();

    let pkg_manifest = pkg.manifest();
    let pkg_path = pkg.manifest_path().parent().expect("Folder exists");
    
    let pkg_name = pkg_manifest.name();
    let doc_uri = pkg_manifest.metadata().documentation.as_ref();

    let mut pkg_source = find_entrypoint(pkg_path)?;
    let readme_path = pkg_path.join("README.md");

    let pkg_readme = fs::read_to_string(readme_path.clone());
    match (mode, pkg_readme) {
        (GenerateReadmeMode::IfMissing, Ok(_existing_readme)) => {
            c.shell()
                .status("Skipping", format!("{}: Readme already exists.", &pkg_name))
                .map_err(|e| format!("{:}", e))?;
            set_readme_field(pkg).map_err(|e| format!("{:}", e))?;
            Ok(())
        }
        (mode, existing_res) => {
            let template_path = find_readme_template(&ws.root(), &pkg_path)?;
            c.shell()
                .status(
                    "Generating",
                    format!(
                        "Readme for {} (template: {:?})",
                        &pkg_name,
                        match &template_path {
                            Some(p) => p.strip_prefix(&root_path).unwrap_or(p).to_str().unwrap(),
                            None => "none found",
                        }
                    ),
                )
                .map_err(|e| format!("{:}", e))?;
            let new_readme = &mut generate_readme(&pkg_path, &mut pkg_source, template_path)?;
            if mode == &GenerateReadmeMode::Append && existing_res.is_ok() {
                *new_readme = format!("{}\n{}", existing_res.unwrap(), new_readme);
            }
            let final_readme = &mut fix_doc_links(&pkg_name, &new_readme, doc_uri.map(|x| x.as_str()));
            let res = fs::write(readme_path, final_readme.as_bytes()).map_err(|e| format!("{:}", e));
            set_readme_field(pkg).map_err(|e| format!("{:}", e))?;
            res
        }
    }
}

fn generate_readme<'a>(
    pkg_path: &Path,
    pkg_source: &mut File,
    template_path: Option<PathBuf>,
) -> Result<String, String> {
    let mut template = template_path
        .map(|p| fs::File::open(&p).expect(&format!("Could not read template at {}", p.display())));

    cargo_readme::generate_readme(
        pkg_path,
        pkg_source,
        template.as_mut(),
        false,
        false,
        true,
        false,
    )
}

fn set_readme_field(pkg: Package) -> Result<(), Box<dyn Error>> {
    commands::set_field(
        vec![pkg].iter(),
        "package".to_owned(),
        "readme".to_owned(),
        Value::from("README.md"),
    )
}

/// Find the default entrypoint to read the doc comments from
///
/// Try to read entrypoint in the following order:
/// - src/lib.rs
/// - src/main.rs
fn find_entrypoint(current_dir: &Path) -> Result<File, String> {
    let entrypoint = find_entrypoint_internal(current_dir)?;
    File::open(current_dir.join(entrypoint)).map_err(|e| format!("{}", e))
}
#[derive(Debug)]
struct ManifestLib {
    pub path: PathBuf,
    pub doc: bool,
}

/// Find the default entrypoint to read the doc comments from
///
/// Try to read entrypoint in the following order:
/// - src/lib.rs
/// - src/main.rs
fn find_entrypoint_internal(current_dir: &Path) -> Result<PathBuf, String> {
    // try lib.rs
    let lib_rs = current_dir.join("src/lib.rs");
    if lib_rs.exists() {
        return Ok(lib_rs);
    }

    // try main.rs
    let main_rs = current_dir.join("src/main.rs");
    if main_rs.exists() {
        return Ok(main_rs);
    }

    // if no entrypoint is found, return an error
    Err("No entrypoint found".to_owned())
}

/// Find the template file to be used to generate README files.
///
/// Start from the package's folder & go up until a template is found
/// (or none).
fn find_readme_template<'a>(
    root_path: &'a Path,
    pkg_path: &'a Path,
) -> Result<Option<PathBuf>, String> {
    let mut cur_path = pkg_path;
    let mut tpl_path = cur_path.join("README.tpl");
    while !tpl_path.exists() && cur_path >= root_path {
        cur_path = cur_path.parent().unwrap();
        tpl_path = cur_path.join("README.tpl");
    }
    Ok(if tpl_path.exists() {
        Some(tpl_path)
    } else {
        None
    })
}

fn fix_doc_links(pkg_name: &str, readme: &str, doc_uri: Option<&str>) -> String {
    RELATIVE_LINKS_REGEX
        .replace_all(&readme, |caps: &Captures| match caps.name("url") {
            Some(url) if url.as_str().starts_with("../") => format!(
                "[{}]({}{})",
                &caps.name("text").unwrap().as_str(),
                doc_uri.unwrap_or(DEFAULT_DOC_URI),
                &url.as_str().replace('_', "-").replace("/index.html", "")[3..]
            ),
            Some(url) if url.as_str().starts_with("./") => format!(
                "[{}]({}{}/latest/{}/{})",
                &caps.name("text").unwrap().as_str(),
                doc_uri.unwrap_or(DEFAULT_DOC_URI),
                pkg_name,
                pkg_name.replace('-', "_"),
                &url.as_str()[2..]
            ),
            _ => caps[0].to_string(),
        })
        .into()
}
