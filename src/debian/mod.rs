use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, ErrorKind, Read, Seek, Write as IoWrite};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

use anyhow::format_err;
use chrono::{self, Datelike};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use regex::Regex;
use tar::{Archive, Builder};
use tempfile;

use crate::config::{force_for_testing, package_field_for_feature, Config, PackageKey};
use crate::crates::{transitive_deps, CrateDepInfo, CrateInfo};
use crate::errors::*;
use crate::util::{
    self, copy_tree, expect_success, get_transitive_val, traverse_depth, vec_opt_iter,
};

use self::changelog::{ChangelogEntry, ChangelogIterator};
use self::control::deb_version;
use self::control::{Description, Package, PkgTest, Source};
use self::copyright::debian_copyright;
pub use self::dependency::{deb_dep_add_nocheck, deb_deps};

pub mod build_order;
pub mod changelog;
pub mod control;
pub mod copyright;
mod dependency;

pub struct DebInfo {
    upstream_name: String,
    base_package_name: String,
    name_suffix: Option<String>, // Some implies semver_suffix, i.e. Some("") is different from None
    uscan_version_pattern: Option<String>,
    package_name: String,
    debian_version: String,
    debcargo_version: String,
    package_source_dir: String,
    orig_tarball_path: String,
}

impl DebInfo {
    pub fn new(
        name: &str,
        crate_info: &CrateInfo,
        debcargo_version: &str,
        semver_suffix: bool,
    ) -> Self {
        let upstream = name.to_string();
        let name_dashed = upstream.replace('_', "-");
        let base_package_name = name_dashed.to_lowercase();
        let (name_suffix, uscan_version_pattern, package_name) = if semver_suffix {
            let semver = crate_info.semver();
            let lib = crate_info.is_lib();
            let bins = crate_info.get_binary_targets();
            let name_suffix = if !lib && !bins.is_empty() {
                "".to_string()
            } else {
                format!("-{}", &semver)
            };

            // See `man uscan` description of @ANY_VERSION@ on how these
            // regex patterns were built.
            let uscan = format!("[-_]?({}\\.\\d[\\-+\\.:\\~\\da-zA-Z]*)", &semver);

            let pkgname = format!("{}{}", base_package_name, &name_suffix);

            (Some(name_suffix), Some(uscan), pkgname)
        } else {
            (None, None, base_package_name.clone())
        };
        let debian_version = deb_version(crate_info.version());
        let debian_source = match name_suffix {
            Some(ref suf) => format!("rust-{}{}", base_package_name, suf),
            None => format!("rust-{}", base_package_name),
        };
        let package_source_dir = format!("{}-{}", debian_source, debian_version);
        let orig_tarball_path = format!("{}_{}.orig.tar.gz", debian_source, debian_version);

        DebInfo {
            upstream_name: upstream,
            base_package_name,
            name_suffix,
            uscan_version_pattern,
            package_name,
            debian_version,
            debcargo_version: debcargo_version.to_string(),
            package_source_dir,
            orig_tarball_path,
        }
    }

    pub fn upstream_name(&self) -> &str {
        self.upstream_name.as_str()
    }

    pub fn base_package_name(&self) -> &str {
        self.base_package_name.as_str()
    }

    pub fn name_suffix(&self) -> Option<&str> {
        self.name_suffix.as_deref()
    }

    pub fn package_name(&self) -> &str {
        self.package_name.as_str()
    }

    pub fn debian_version(&self) -> &str {
        self.debian_version.as_str()
    }

    pub fn debcargo_version(&self) -> &str {
        self.debcargo_version.as_str()
    }

    pub fn package_source_dir(&self) -> &str {
        self.package_source_dir.as_str()
    }

    pub fn orig_tarball_path(&self) -> &str {
        self.orig_tarball_path.as_str()
    }
}

pub fn prepare_orig_tarball(
    crate_info: &CrateInfo,
    tarball: &Path,
    src_modified: bool,
    pkg_srcdir: &Path,
) -> Result<()> {
    let crate_file = crate_info.crate_file();
    let tempdir = tempfile::Builder::new()
        .prefix("debcargo")
        .tempdir_in(".")?;
    let temp_archive_path = tempdir.path().join(tarball);

    let mut create = fs::OpenOptions::new();
    create.write(true).create_new(true);

    if src_modified {
        debcargo_info!("crate tarball was modified; repacking for debian");
        let mut f = crate_file.file();
        f.seek(io::SeekFrom::Start(0))?;
        let mut archive = Archive::new(GzDecoder::new(f));
        let mut new_archive = Builder::new(GzEncoder::new(
            create.open(&temp_archive_path)?,
            Compression::best(),
        ));

        for entry in archive.entries()? {
            let entry = entry?;
            let path = entry.path()?.into_owned();
            if path.ends_with("Cargo.toml") && path.iter().count() == 2 {
                // Put the rewritten and original Cargo.toml back into the orig tarball
                let mut new_archive_append = |name: &str| {
                    let mut header = entry.header().clone();
                    let srcpath = pkg_srcdir.join(name);
                    header.set_path(path.parent().unwrap().join(name))?;
                    header.set_size(fs::metadata(&srcpath)?.len());
                    header.set_cksum();
                    new_archive.append(&header, fs::File::open(&srcpath)?)
                };
                new_archive_append("Cargo.toml")?;
                new_archive_append("Cargo.toml.orig")?;
            } else {
                match crate_info.filter_path(&entry.path()?) {
                    Err(e) => debcargo_bail!(e),
                    Ok(r) => {
                        if !r {
                            new_archive.append(&entry.header().clone(), entry)?;
                        } else {
                            writeln!(
                                io::stderr(),
                                "Filtered out files from .orig.tar.gz: {:?}",
                                &entry.path()?
                            )?;
                        }
                    }
                }
            }
        }

        new_archive.finish()?;
    } else {
        fs::copy(crate_file.path(), &temp_archive_path)?;
    }

    fs::rename(temp_archive_path, &tarball)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_debian_folder(
    deb_info: &DebInfo,
    crate_info: &mut CrateInfo,
    pkg_srcdir: &Path,
    config_path: Option<&Path>,
    config: &Config,
    changelog_ready: bool,
    copyright_guess_harder: bool,
    overlay_write_back: bool,
) -> Result<()> {
    let tempdir = tempfile::Builder::new()
        .prefix("debcargo")
        .tempdir_in(".")?;
    let overlay = config.overlay_dir(config_path);
    if let Some(p) = overlay.as_ref() {
        if p.to_str().unwrap() == "." {
            debcargo_bail!(
                "Aborting: refusing to recursively copy {} to {} \
(overlay directory should not be .)",
                p.to_str().unwrap(),
                tempdir.path().to_str().unwrap()
            );
        }
        copy_tree(p.as_path(), tempdir.path()).unwrap();
    }
    if tempdir.path().join("control").exists() {
        debcargo_warn!(
            "Most of the time you shouldn't overlay debian/control, \
it's a maintenance burden. Use debcargo.toml instead."
        )
    }
    if tempdir.path().join("patches").join("series").exists() {
        // apply patches to Cargo.toml in case they exist, and re-read it
        let pkg_srcdir = &fs::canonicalize(&pkg_srcdir)?;
        expect_success(
            Command::new("quilt")
                .current_dir(&pkg_srcdir)
                .env("QUILT_PATCHES", tempdir.path().join("patches"))
                .args(&["push", "--quiltrc=-", "-a"]),
            "failed to apply patches",
        );
        crate_info.replace_manifest(&pkg_srcdir.join("Cargo.toml"))?;
        expect_success(
            Command::new("quilt")
                .current_dir(&pkg_srcdir)
                .env("QUILT_PATCHES", tempdir.path().join("patches"))
                .args(&["pop", "--quiltrc=-", "-a"]),
            "failed to unapply patches",
        );
    }

    let mut create = fs::OpenOptions::new();
    create.write(true).create_new(true);

    let crate_name = crate_info.package_id().name();
    let crate_version = crate_info.package_id().version();
    let upstream_name = deb_info.upstream_name();

    let maintainer = config.maintainer();
    let uploaders: Vec<&str> = vec_opt_iter(config.uploaders())
        .map(String::as_str)
        .collect();

    let mut new_hints = vec![];
    let mut file = |name: &str| {
        let path = tempdir.path();
        let f = path.join(name);
        fs::create_dir_all(f.parent().unwrap())?;
        create.open(&f).or_else(|e| match e.kind() {
            ErrorKind::AlreadyExists => {
                let hintname = name.to_owned() + util::HINT_SUFFIX;
                let hint = path.join(&hintname);
                if hint.exists() {
                    fs::remove_file(&hint)?;
                }
                new_hints.push(hintname);
                create.open(&hint)
            }
            _ => Err(e),
        })
    };

    // debian/cargo-checksum.json
    {
        let checksum = crate_info
            .checksum()
            .unwrap_or("Could not get crate checksum");
        let mut cargo_checksum_json = file("cargo-checksum.json")?;
        writeln!(
            cargo_checksum_json,
            r#"{{"package":"{}","files":{{}}}}"#,
            checksum
        )?;
    }

    // debian/compat
    {
        let mut compat = file("compat")?;
        writeln!(compat, "12")?;
    }

    // debian/copyright
    {
        let mut copyright = io::BufWriter::new(file("copyright")?);
        let year_range = if changelog_ready {
            // if changelog is ready, unconditionally read the year range from it
            changelog_first_last(tempdir.path())?
        } else {
            // otherwise use the first date if it exists
            let last = chrono::Local::now().year();
            match changelog_first_last(tempdir.path()) {
                Ok((first, _)) => (first, last),
                Err(_) => (last, last),
            }
        };
        let dep5_copyright = debian_copyright(
            crate_info.package(),
            pkg_srcdir,
            crate_info.manifest(),
            maintainer,
            &uploaders,
            year_range,
            copyright_guess_harder,
        )?;
        write!(copyright, "{}", dep5_copyright)?;
    }

    // debian/watch
    {
        let mut watch = file("watch")?;
        match config.crate_src_path(config_path) {
            Some(_) => write!(watch, "FIXME add uscan directive for local crate")?,
            None => {
                let uscan_version_pattern = deb_info
                    .uscan_version_pattern
                    .as_ref()
                    .map_or_else(|| "@ANY_VERSION@".to_string(), |ref s| s.to_string());
                writeln!(watch, "version=4")?;
                writeln!(
                    watch,
                    r"opts=filenamemangle=s/.*\/(.*)\/download/{name}-$1\.tar\.gz/g,\",
                    name = upstream_name
                )?;
                writeln!(
                    watch,
                    r"uversionmangle=s/(\d)[_\.\-\+]?((RC|rc|pre|dev|beta|alpha)\d*)$/$1~$2/ \"
                )?;
                writeln!(
                    watch,
                    "https://qa.debian.org/cgi-bin/fakeupstream.cgi?upstream=crates.io/{name} \
                     .*/crates/{name}/{version_pattern}/download",
                    name = upstream_name,
                    version_pattern = uscan_version_pattern
                )?;
            }
        };
    }

    // debian/source/format
    {
        fs::create_dir_all(tempdir.path().join("source"))?;
        let mut source_format = file("source/format")?;
        writeln!(source_format, "3.0 (quilt)")?;
    }

    // debian/control & debian/tests/control
    let (source, has_dev_depends, default_test_broken) =
        prepare_debian_control(deb_info, crate_info, config, &mut file)?;

    // for testing only, debian/debcargo_testing_bin/env
    if force_for_testing() {
        fs::create_dir_all(tempdir.path().join("debcargo_testing_bin"))?;
        let mut env_hack = file("debcargo_testing_bin/env")?;
        env_hack.set_permissions(fs::Permissions::from_mode(0o777))?;
        // intercept calls to dh-cargo-built-using
        writeln!(
            env_hack,
            r#"#!/bin/sh
case "$*" in */usr/share/cargo/bin/dh-cargo-built-using*)
echo "debcargo testing: suppressing dh-cargo-built-using";;
*) /usr/bin/env "$@";; esac
"#
        )?;
    }

    // debian/rules
    {
        let mut rules = file("rules")?;
        rules.set_permissions(fs::Permissions::from_mode(0o777))?;
        if has_dev_depends || force_for_testing() {
            // don't run any tests, we don't want extra B-D on dev-depends
            // this could potentially cause B-D cycles so we avoid it
            //
            // also don't run crate tests during integration testing since some
            // of them are brittle and fail; the purpose is to test debcargo
            // not the actual crates
            write!(
                rules,
                "{}",
                concat!(
                    "#!/usr/bin/make -f\n",
                    "%:\n",
                    "\tdh $@ --buildsystem cargo\n"
                )
            )?;
            // some crates need nightly to compile, annoyingly. only do this in
            // testing; outside of testing the user should explicitly override
            // debian/rules to do this
            if force_for_testing() {
                writeln!(rules, "export RUSTC_BOOTSTRAP := 1")?;
                writeln!(
                    rules,
                    "export PATH := $(CURDIR)/debian/debcargo_testing_bin:$(PATH)"
                )?;
            }
        } else {
            write!(
                rules,
                "{}{}",
                concat!(
                    "#!/usr/bin/make -f\n",
                    "%:\n",
                    "\tdh $@ --buildsystem cargo\n",
                    "\n",
                    "override_dh_auto_test:\n",
                ),
                // TODO: this logic is slightly brittle if another feature
                // "provides" the default feature. In this case, you need to
                // set test_is_broken explicitly on package."lib+default" and
                // not package."lib+theotherfeature".
                if default_test_broken {
                    "\tdh_auto_test -- test --all || true\n"
                } else {
                    "\tdh_auto_test -- test --all\n"
                },
            )?;
        }
    }

    // debian/changelog
    if !changelog_ready {
        let author = control::get_deb_author()?;
        let crate_src = match config.crate_src_path(config_path) {
            Some(_) => "local source",
            None => "crates.io",
        };
        let autogenerated_item = format!(
            "  * Package {} {} from {} using debcargo {}",
            &crate_name,
            &crate_version,
            &crate_src,
            deb_info.debcargo_version()
        );
        let autogenerated_re = Regex::new(&format!(
            r"^  \* Package (.*) (.*) from {} using debcargo (.*)$",
            &crate_src
        ))
        .unwrap();

        // Special-case d/changelog:
        let (mut changelog, changelog_data) = changelog_or_new(tempdir.path())?;
        let (changelog_old, mut changelog_items, deb_version_suffix) = {
            let ver_bump = &|e: &Option<&str>| -> Result<Option<String>> {
                Ok(match e {
                    Some(x) => {
                        let e = ChangelogEntry::from_str(x)?;
                        if e.version_parts().0 == deb_info.debian_version() {
                            Some(e.deb_version_suffix_bump())
                        } else {
                            None
                        }
                    }
                    None => None,
                })
            };
            let mut chit = ChangelogIterator::from(&changelog_data);
            let e1 = chit.next();
            match e1 {
                // If the first entry has changelog::DEFAULT_DIST then write over it smartly
                Some(x) if x.contains(changelog::DEFAULT_DIST) => {
                    let mut e = ChangelogEntry::from_str(x)?;
                    if author == e.maintainer {
                        if let Some(pos) = e.items.iter().position(|x| autogenerated_re.is_match(x))
                        {
                            e.items[pos] = autogenerated_item;
                        } else {
                            e.items.push(autogenerated_item);
                        }
                    } else {
                        // If unreleased changelog is by someone else, preserve their entries
                        e.items.insert(0, autogenerated_item);
                        e.items.insert(1, "".to_string());
                        let ename = e.maintainer_name();
                        e.items.insert(2, format!("  [ {} ]", ename));
                    }
                    (&changelog_data[x.len()..], e.items, ver_bump(&chit.next())?)
                }
                // Otherwise prepend a new entry to the existing entries
                _ => (
                    changelog_data.as_str(),
                    vec![autogenerated_item],
                    ver_bump(&e1)?,
                ),
            }
        };

        let source_deb_version = format!(
            "{}-{}",
            deb_info.debian_version(),
            &deb_version_suffix.unwrap_or_else(|| "1".to_string())
        );
        if !uploaders.contains(&author.as_str()) {
            debcargo_warn!(
                "You ({}) are not in Uploaders; adding \"Team upload\" to d/changelog",
                author
            );
            if !changelog_items.contains(&changelog::COMMENT_TEAM_UPLOAD.to_string()) {
                changelog_items.insert(0, changelog::COMMENT_TEAM_UPLOAD.to_string());
            }
        }
        let changelog_new_entry = ChangelogEntry::new(
            source.name().to_string(),
            source_deb_version,
            changelog::DEFAULT_DIST.to_string(),
            "urgency=medium".to_string(),
            author,
            changelog::local_now(),
            changelog_items,
        );

        changelog.seek(io::SeekFrom::Start(0))?;
        if changelog_old.is_empty() {
            write!(changelog, "{}", changelog_new_entry)?;
        } else {
            write!(changelog, "{}\n{}", changelog_new_entry, changelog_old)?;
        }
        // the new file might be shorter, truncate it to the current cursor position
        let pos = changelog.seek(io::SeekFrom::Current(0))?;
        changelog.set_len(pos)?;
    }

    if overlay_write_back {
        if let Some(p) = overlay.as_ref() {
            if !changelog_ready {
                // Special-case d/changelog:
                // Always write it back, this is safe because of our prepending logic
                new_hints.push("changelog".to_string());
            }
            for hint in &new_hints {
                let newpath = tempdir.path().join(hint);
                let oldpath = p.join(hint);
                fs::copy(newpath, oldpath).expect("could not write back");
                debcargo_info!("Wrote back file to overlay: {}", hint);
            }
        }
    }

    fs::rename(tempdir.path(), pkg_srcdir.join("debian"))?;
    Ok(())
}

fn prepare_debian_control<F: FnMut(&str) -> std::result::Result<std::fs::File, std::io::Error>>(
    deb_info: &DebInfo,
    crate_info: &CrateInfo,
    config: &Config,
    mut file: F,
) -> Result<(Source, bool, bool)> {
    let lib = crate_info.is_lib();
    let mut bins = crate_info.get_binary_targets();
    if lib && !bins.is_empty() && !config.build_bin_package() {
        bins.clear();
    }
    let default_bin_name = crate_info.package().name().to_string().replace('_', "-");
    let bin_name = if config.bin_name.eq(&Config::default().bin_name) {
        if !bins.is_empty() {
            debcargo_info!(
                "Generate binary crate with default name '{}', set bin_name to override or bin = false to disable.",
                &default_bin_name
            );
        }
        &default_bin_name
    } else {
        &config.bin_name
    };

    let crate_name = crate_info.package_id().name();
    let debian_version = deb_info.debian_version();
    let base_pkgname = deb_info.base_package_name();
    let name_suffix = deb_info.name_suffix();
    let upstream_name = deb_info.upstream_name();

    let maintainer = config.maintainer();
    let requires_root = config.requires_root();
    let uploaders: Vec<&str> = vec_opt_iter(config.uploaders())
        .map(String::as_str)
        .collect();

    let features_with_deps = crate_info.all_dependencies_and_features();
    let dev_depends = deb_deps(config, &crate_info.dev_dependencies())?;
    /*debcargo_info!(
        "features_with_deps: {:?}",
        features_with_deps
            .iter()
            .map(|(&f, &(ref ff, ref dd))| { (f, (ff, deb_deps(config, dd).unwrap())) })
            .collect::<Vec<_>>()
    );*/
    let meta = crate_info.metadata();

    // debian/tests/control, preparation
    let test_is_marked_broken = |f: &str| config.package_test_is_broken(PackageKey::feature(f));
    let test_is_broken = |f: &str| {
        let getparents = |f: &str| features_with_deps.get(f).map(|(d, _)| d);
        match get_transitive_val(&getparents, &test_is_marked_broken, f) {
            Err((k, vv)) => debcargo_bail!(
                "{} {}: {}: {:?}",
                "error trying to recursively determine test_is_broken for",
                k,
                "dependencies have inconsistent config values",
                vv
            ),
            Ok(v) => Ok(v.unwrap_or(false)),
        }
    };

    let build_deps = {
        let build_deps = ["debhelper (>= 12)", "dh-cargo (>= 25)"]
            .iter()
            .map(|x| x.to_string());
        // note: please keep this in sync with build_order::dep_features
        let (default_features, default_deps) = transitive_deps(&features_with_deps, "default");
        //debcargo_info!("default_features: {:?}", default_features);
        //debcargo_info!("default_deps: {:?}", deb_deps(config, &default_deps)?);
        let extra_override_deps = package_field_for_feature(
            &|x| config.package_depends(x),
            PackageKey::feature("default"),
            &default_features,
        );
        let build_deps_extra = ["cargo:native", "rustc:native", "libstd-rust-dev"]
            .iter()
            .map(|s| s.to_string())
            .chain(deb_deps(config, &default_deps)?)
            .chain(extra_override_deps);
        if !bins.is_empty() {
            build_deps.chain(build_deps_extra).collect()
        } else {
            assert!(lib);
            build_deps
                .chain(build_deps_extra.map(|d| deb_dep_add_nocheck(&d)))
                .collect()
        }
    };
    let mut source = Source::new(
        base_pkgname,
        name_suffix,
        upstream_name,
        if let Some(ref home) = meta.homepage {
            home
        } else {
            ""
        },
        lib,
        maintainer.to_string(),
        uploaders.iter().map(|s| s.to_string()).collect(),
        build_deps,
        if requires_root.is_some() {
            requires_root.as_ref().unwrap().to_string()
        } else {
            "no".to_string()
        },
    )?;

    // If source overrides are present update related parts.
    source.apply_overrides(config);

    let mut control = io::BufWriter::new(file("control")?);
    write!(control, "{}", source)?;

    // Summary and description generated from Cargo.toml
    let (crate_summary, crate_description) = crate_info.get_summary_description();
    let summary_prefix = crate_summary.unwrap_or(format!("Rust crate \"{}\"", upstream_name));
    let description_prefix = {
        let tmp = crate_description.unwrap_or_else(|| "".to_string());
        if tmp.is_empty() {
            tmp
        } else {
            format!("{}\n.\n", tmp)
        }
    };

    if lib {
        // debian/tests/control
        let all_features_test_broken = Some(&"@")
            .into_iter()
            .chain(features_with_deps.keys())
            .any(|f| test_is_marked_broken(f).unwrap_or(false));
        let all_features_test_depends = Some(&"@")
            .into_iter()
            .chain(features_with_deps.keys())
            .map(|f| vec_opt_iter(config.package_test_depends(PackageKey::feature(f))))
            .flatten()
            .map(|s| s.to_string())
            .chain(dev_depends.clone())
            .collect::<Vec<_>>();
        let mut testctl = io::BufWriter::new(file("tests/control")?);
        write!(
            testctl,
            "{}",
            PkgTest::new(
                source.name(),
                &crate_name,
                "@",
                debian_version,
                vec!["--all-features"],
                &all_features_test_depends,
                if all_features_test_broken {
                    vec!["flaky"]
                } else {
                    vec![]
                },
            )?
        )?;

        let (mut provides, reduced_features_with_deps) = if config.collapse_features {
            debcargo_warn!(
                "You are using the collapse_features work-around, which makes the resulting"
            );
            debcargo_warn!(
                "package uninstallable when (now or in the future) your crate dependencies"
            );
            debcargo_warn!(
                "contain cyclic dependencies on the crate-level; this is because cargo only"
            );
            debcargo_warn!("enforces acyclicity of dependencies on the per-feature level.");
            debcargo_warn!("");
            debcargo_warn!(
                "By switching on collapse_features, you are telling debcargo to generate Debian"
            );
            debcargo_warn!(
                "binary package on a per-crate-level basis and not a per-feature-level, meaning"
            );
            debcargo_warn!(
                "that there is the chance of generating a dependency cycle on the Debian binary"
            );
            debcargo_warn!("package level, which APT by default refuses to install.");
            debcargo_warn!("");
            debcargo_warn!(
                "You should not be doing this just because \"somebody told you so\"; you should"
            );
            debcargo_warn!(
                "understand the situation and be prepared to deal with future technical debt"
            );
            debcargo_warn!("when the aforementioned cycles arise.");
            debcargo_warn!("");
            debcargo_warn!(
                "Note that a long-term solution has been discussed with the FTP team and is in"
            );
            debcargo_warn!(
                "progress - namely to move Debian rust packages into a separate section of the"
            );
            debcargo_warn!(
                "archive, which will then have the stricter current NEW rules lifted, and then"
            );
            debcargo_warn!("the collapse_features work around would no longer be necessary.");
            debcargo_warn!("");
            debcargo_warn!("A basic example of the above would be:");
            debcargo_warn!("");
            debcargo_warn!("- crate A with feature AX depends on crate B with feature BY");
            debcargo_warn!("- crate B with feature BX depends on crate A with feature AY");
            debcargo_warn!("");
            debcargo_warn!(
                "This is a perfectly valid situation in the rust+cargo ecosystem. Notice that"
            );
            debcargo_warn!(
                "there is no dependency cycle on the per-feature level, and this is enforced by"
            );
            debcargo_warn!(
                "cargo; but if collapse_features is used then package A+AX+AY would cyclicly"
            );
            debcargo_warn!("depend on package B+BX+BY.");
            collapse_features(&features_with_deps)
        } else {
            reduce_provides(&features_with_deps)
        };

        //debcargo_info!("provides: {:?}", provides);
        let mut recommends = vec![];
        let mut suggests = vec![];
        for (&feature, features) in provides.iter() {
            if feature.is_empty() {
                continue;
            } else if feature == "default" || features.contains(&"default") {
                recommends.push(feature);
            } else {
                suggests.push(feature);
            }
        }

        for (feature, (f_deps, o_deps)) in reduced_features_with_deps.into_iter() {
            let pk = PackageKey::feature(feature);
            let f_provides = provides.remove(feature).unwrap();
            let mut crate_features = f_provides.clone();
            crate_features.push(feature);

            let summary_suffix = if feature.is_empty() {
                " - Rust source code".to_string()
            } else {
                match f_provides.len() {
                    0 => format!(" - feature \"{}\"", feature),
                    _ => format!(" - feature \"{}\" and {} more", feature, f_provides.len()),
                }
            };
            let description_suffix = if feature.is_empty() {
                format!(
                    "This package contains the source for the \
                     Rust {} crate, packaged by debcargo for use \
                     with cargo and dh-cargo.",
                    upstream_name
                )
            } else {
                format!(
                    "This metapackage enables feature \"{}\" for the \
                     Rust {} crate, by pulling in any additional \
                     dependencies needed by that feature.{}",
                    feature,
                    upstream_name,
                    match f_provides.len() {
                        0 => "".to_string(),
                        1 => format!(
                            "\n\nAdditionally, this package also provides the \
                             \"{}\" feature.",
                            f_provides[0],
                        ),
                        _ => format!(
                            "\n\nAdditionally, this package also provides the \
                             \"{}\", and \"{}\" features.",
                            f_provides[..f_provides.len() - 1].join("\", \""),
                            f_provides[f_provides.len() - 1],
                        ),
                    },
                )
            };
            let mut package = Package::new(
                base_pkgname,
                name_suffix,
                crate_info.version(),
                Description {
                    prefix: summary_prefix.clone(),
                    suffix: summary_suffix.clone(),
                },
                Description {
                    prefix: description_prefix.clone(),
                    suffix: description_suffix.clone(),
                },
                if feature.is_empty() {
                    None
                } else {
                    Some(feature)
                },
                f_deps,
                deb_deps(config, &o_deps)?,
                f_provides.clone(),
                if feature.is_empty() {
                    recommends.clone()
                } else {
                    vec![]
                },
                if feature.is_empty() {
                    suggests.clone()
                } else {
                    vec![]
                },
            )?;
            // If any overrides present for this package it will be taken care.
            package.apply_overrides(config, pk, f_provides);

            match package.summary_check_len() {
                Err(()) => writeln!(
                    control,
                    concat!(
                        "\n",
                        "# FIXME (packages.\"(name)\".section) debcargo ",
                        "auto-generated summary for {} is very long, consider overriding"
                    ),
                    package.name(),
                )?,
                Ok(()) => {}
            };

            write!(control, "\n{}", package)?;

            // Override pointless overzealous warnings from lintian
            if !feature.is_empty() {
                let mut overrides =
                    io::BufWriter::new(file(&format!("{}.lintian-overrides", package.name()))?);
                write!(
                    overrides,
                    "{} binary: empty-rust-library-declares-provides *",
                    package.name()
                )?;
            }

            // Generate tests for all features in this package
            for f in crate_features {
                let (feature_deps, _) = transitive_deps(&features_with_deps, f);

                // args
                let mut args = if f == "default" || feature_deps.contains(&"default") {
                    vec![]
                } else {
                    vec!["--no-default-features"]
                };
                // --features default sometimes fails, see
                // https://github.com/rust-lang/cargo/issues/8164
                if !f.is_empty() && f != "default" {
                    args.push("--features");
                    args.push(f);
                }

                // deps
                let test_depends = Some(f)
                    .into_iter()
                    .chain(feature_deps)
                    .map(|f| vec_opt_iter(config.package_test_depends(PackageKey::feature(f))))
                    .flatten()
                    .map(|s| s.to_string())
                    .chain(dev_depends.clone())
                    .collect::<Vec<_>>();
                let pkgtest = PkgTest::new(
                    package.name(),
                    &crate_name,
                    f,
                    debian_version,
                    args,
                    &test_depends,
                    if test_is_broken(f)? {
                        vec!["flaky"]
                    } else {
                        vec![]
                    },
                )?;
                write!(testctl, "\n{}", pkgtest)?;
            }
        }
        assert!(provides.is_empty());
        // reduced_features_with_deps consumed by into_iter, no longer usable
    }

    if !bins.is_empty() {
        // adding " - binaries" is a bit redundant for users, so just leave as-is
        let summary_suffix = "".to_string();
        let description_suffix = format!(
            "This package contains the following binaries built from the Rust crate\n\"{}\":\n - {}",
            upstream_name,
            bins.join("\n - ")
        );

        let mut bin_pkg = Package::new_bin(
            bin_name,
            name_suffix,
            // if not-a-lib then Source section is already FIXME
            if !lib {
                None
            } else {
                Some("FIXME-(packages.\"(name)\".section)")
            },
            Description {
                prefix: summary_prefix,
                suffix: summary_suffix,
            },
            Description {
                prefix: description_prefix,
                suffix: description_suffix,
            },
        );

        // Binary package overrides.
        bin_pkg.apply_overrides(config, PackageKey::Bin, vec![]);
        write!(control, "\n{}", bin_pkg)?;
    }

    Ok((source, !dev_depends.is_empty(), test_is_broken("default")?))
}

fn collapse_features(
    orig_features_with_deps: &CrateDepInfo,
) -> (BTreeMap<&'static str, Vec<&'static str>>, CrateDepInfo) {
    let (provides, deps) = orig_features_with_deps.iter().fold(
        (Vec::new(), Vec::new()),
        |(mut provides, mut deps), (f, (_, f_deps))| {
            if f != &"" {
                provides.push(*f);
            }
            deps.append(&mut f_deps.clone());
            (provides, deps)
        },
    );

    let mut collapsed_provides = BTreeMap::new();
    collapsed_provides.insert("", provides);

    let mut collapsed_features_with_deps = BTreeMap::new();
    collapsed_features_with_deps.insert("", (Vec::new(), deps));

    (collapsed_provides, collapsed_features_with_deps)
}

/// Calculate Provides: in an attempt to reduce the number of binaries.
///
/// The algorithm is very simple and incomplete. e.g. it does not, yet
/// simplify things like:
///   f1 depends on f2, f3
///   f2 depends on f4
///   f3 depends on f4
/// into
///   f4 provides f1, f2, f3
fn reduce_provides(
    orig_features_with_deps: &CrateDepInfo,
) -> (BTreeMap<&'static str, Vec<&'static str>>, CrateDepInfo) {
    let mut features_with_deps = orig_features_with_deps.clone();

    // If any features have duplicate dependencies, deduplicate them by
    // making all of the subsequent ones depend on the first one.
    let mut features_rev_deps = HashMap::new();
    for (&f, dep) in features_with_deps.iter() {
        if !features_rev_deps.contains_key(dep) {
            features_rev_deps.insert(dep.clone(), vec![]);
        }
        features_rev_deps.get_mut(dep).unwrap().push(f);
    }
    for (_, ff) in features_rev_deps.into_iter() {
        let f0 = ff[0];
        for f in &ff[1..] {
            features_with_deps.insert(f, (vec![f0], vec![]));
        }
    }

    // Calculate provides by following 0- or 1-length dependency lists.
    let mut provides = BTreeMap::new();
    let mut provided = Vec::new();
    for (&f, (ref ff, ref dd)) in features_with_deps.iter() {
        //debcargo_info!("provides considering: {:?}", &f);
        if !dd.is_empty() {
            continue;
        }
        assert!(!ff.is_empty() || f.is_empty());
        let k = if ff.len() == 1 {
            // if A depends on a single feature B, then B provides A.
            ff[0]
        } else {
            continue;
        };
        //debcargo_info!("provides still considering: {:?}", &f);
        if !provides.contains_key(k) {
            provides.insert(k, vec![]);
        }
        provides.get_mut(k).unwrap().push(f);
        provided.push(f);
    }

    //debcargo_info!("provides-internal: {:?}", &provides);
    //debcargo_info!("provided-internal: {:?}", &provided);
    for p in provided {
        features_with_deps.remove(p);
    }

    let provides = features_with_deps
        .keys()
        .map(|k| {
            let mut pp = traverse_depth(&provides, k);
            pp.sort_unstable();
            (*k, pp)
        })
        .collect::<BTreeMap<_, _>>();

    (provides, features_with_deps)
}

fn changelog_or_new(tempdir: &Path) -> Result<(fs::File, String)> {
    let mut changelog = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(tempdir.join("changelog"))?;
    let mut changelog_data = String::new();
    changelog.read_to_string(&mut changelog_data)?;
    Ok((changelog, changelog_data))
}

fn changelog_first_last(tempdir: &Path) -> Result<(i32, i32)> {
    let mut changelog = fs::File::open(tempdir.join("changelog"))?;
    let mut changelog_data = String::new();
    changelog.read_to_string(&mut changelog_data)?;
    let mut last = None;
    let mut first = None;
    for x in ChangelogIterator::from(&changelog_data) {
        let e = ChangelogEntry::from_str(x)?;
        if None == last {
            last = Some(e.date.year());
        }
        first = Some(e.date.year());
    }
    if None == last {
        Err(format_err!("changelog had no entries"))
    } else {
        Ok((first.unwrap(), last.unwrap()))
    }
}
