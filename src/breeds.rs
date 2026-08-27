//! Breeds (品种) — one named set of ZeroClaw templates.
//!
//! A breed is what makes two tenants on the same ClawOps box different
//! kinds of lobster: its own `IDENTITY.md.hbs` / `SOUL.md.hbs` /
//! `USER.md.hbs` / `config.toml.hbs`, plus `skills/`, `sops/` and
//! `scripts/` trees. `users.breed` binds a tenant to one; the provisioner
//! renders that tenant's workspace from it and nothing else.
//!
//! Breeds arrive as a **bundle**: a (optionally gzipped) tar of the
//! template tree, pushed by whatever developed the lobster. The upload
//! path is deliberately paranoid — it lands in a staging directory, is
//! validated in full, and only then swaps into place. A bundle that
//! fails any check leaves the live breed exactly as it was, because the
//! alternative is a swarm of tenants rendering from a half-written tree.

use crate::config::{Config, DEFAULT_BREED};
use crate::{Error, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// Without this the daemon has no config to start from, so a bundle
/// missing it would provision tenants that never come up.
const REQUIRED_FILES: &[&str] = &["config.toml.hbs"];

/// Refused outright regardless of `max_bundle_bytes`: 20k entries is far
/// past any real template tree and well short of what it takes to spend
/// meaningful time in the extract loop.
const MAX_ENTRIES: usize = 20_000;

/// Staging and trash directories live inside `breeds_dir` (same
/// filesystem, so the swap is a rename rather than a copy) and start
/// with a dot so they can never collide with a breed name.
const STAGING_PREFIX: &str = ".staging-";
const TRASH_PREFIX: &str = ".trash-";

#[derive(Debug, Clone, Serialize)]
pub struct BreedInfo {
    pub name: String,
    /// Absolute path of the template directory backing this breed.
    pub path: String,
    /// Number of files in the tree.
    pub files: usize,
    /// sha256 over the sorted `path -> content-hash` manifest. Two
    /// deployments showing the same digest are running byte-identical
    /// templates — which is the question anyone pushing a breed
    /// actually wants answered.
    pub digest: String,
    /// True for the breed backed by `provisioner.template_dir`. It is
    /// read-only over the API: it ships with the binary, so letting a
    /// push overwrite it would make the box's own git checkout a lie.
    pub builtin: bool,
    /// How many tenants currently render from this breed.
    pub tenants: i64,
}

/// Breed names go into filesystem paths and URLs. Keep them boring.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(Error::BadBundle(format!(
            "breed name must be 1-64 chars, got {}",
            name.len()
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(Error::BadBundle(format!(
            "breed name '{name}' must match [a-z0-9_-]+"
        )));
    }
    if name.starts_with('-') || name.starts_with('_') {
        return Err(Error::BadBundle(format!(
            "breed name '{name}' must not start with a separator"
        )));
    }
    Ok(())
}

fn breeds_root(cfg: &Config) -> Result<&Path> {
    cfg.provisioner
        .breeds_dir
        .as_deref()
        .ok_or(Error::BreedsDisabled)
}

/// `path -> sha256(content)` for every regular file under `dir`,
/// relative to `dir`, sorted. Directories with no files are invisible
/// here, which is correct: an empty directory changes no behaviour.
pub fn manifest(dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    // A breed pointed at a directory that isn't there is a real state to
    // report (`files: 0`), not an error to raise — that is exactly the
    // case an operator is listing breeds to discover.
    if dir.is_dir() {
        walk(dir, dir, &mut out)?;
    }
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // `file_type` does not follow symlinks, so a link is neither a
        // dir nor a file here and drops out of the manifest — matching
        // the extractor, which refuses to create one in the first place.
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&path)?;
            out.insert(rel, hex(&Sha256::digest(&bytes)));
        }
    }
    Ok(())
}

/// One hash standing for the whole tree.
pub fn digest_of(manifest: &BTreeMap<String, String>) -> String {
    let mut h = Sha256::new();
    for (path, file_hash) in manifest {
        h.update(path.as_bytes());
        h.update(b"\0");
        h.update(file_hash.as_bytes());
        h.update(b"\n");
    }
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Describe one breed, or `None` when nothing backs that name.
pub fn describe(cfg: &Config, name: &str, tenants: i64) -> Result<Option<BreedInfo>> {
    let Some(dir) = cfg.provisioner.breed_dir(name) else {
        return Ok(None);
    };
    let m = manifest(&dir)?;
    Ok(Some(BreedInfo {
        name: name.to_string(),
        path: dir.to_string_lossy().to_string(),
        files: m.len(),
        digest: digest_of(&m),
        builtin: name == cfg.provisioner.default_breed || name == DEFAULT_BREED,
        tenants,
    }))
}

/// Every breed the box can render: the built-in one plus each directory
/// under `breeds_dir`. `tenant_counts` comes from the DB so the listing
/// can show which breeds are actually in use.
pub fn list(cfg: &Config, tenant_counts: &BTreeMap<String, i64>) -> Result<Vec<BreedInfo>> {
    let mut names = vec![cfg.provisioner.default_breed.clone()];
    if let Some(root) = cfg.provisioner.breeds_dir.as_deref() {
        if root.is_dir() {
            for entry in std::fs::read_dir(root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                // Skip our own staging/trash scratch dirs.
                if name.starts_with('.') || names.contains(&name) {
                    continue;
                }
                names.push(name);
            }
        }
    }
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let tenants = tenant_counts.get(&name).copied().unwrap_or(0);
        if let Some(info) = describe(cfg, &name, tenants)? {
            out.push(info);
        }
    }
    Ok(out)
}

/// Install a bundle as `breed`, replacing whatever was there.
///
/// The bundle is a tar, gzipped or not, whose entries are the breed
/// directory's contents (`config.toml.hbs`, `skills/…`) — a leading
/// `./` is tolerated because that is what `tar -C dir -czf - .` emits.
///
/// Nothing touches the live directory until the staged tree has passed
/// every check, including a handlebars parse of every `.hbs` file. A
/// template that fails to compile is the single most common way a push
/// breaks a swarm, and it costs nothing to catch here instead of at the
/// next provision.

/// Read every file of a staged tree as text, for the linter. Binary assets
/// (fonts, images under `scripts/`) are skipped rather than failing: they
/// are legitimate breed content and nothing in the lint rules reads them.
pub fn read_sources(dir: &Path) -> Result<std::collections::BTreeMap<String, String>> {
    let mut out = std::collections::BTreeMap::new();
    for rel in manifest(dir)?.keys() {
        if let Ok(text) = std::fs::read_to_string(dir.join(rel)) {
            out.insert(rel.clone(), text);
        }
    }
    Ok(out)
}

/// Install a bundle, but only after checking that it renders into something
/// a tenant can actually run.
///
/// `validate_tree` proves the templates parse. That is not the same as
/// proving they *work*: the first hand-authored breed compiled cleanly and
/// still shipped a model name pointed at the wrong endpoint, a skill whose
/// instructions the model was forbidden to read, and a knowledge file that
/// every render would delete. None of those are typos, so none of them were
/// catchable before rendering — which is what this does, using the same
/// context a real tenant gets.
///
/// Findings at `Error` level abort before anything is written. Warnings are
/// returned for the caller to surface. With `dry_run` nothing is installed
/// either way, so the development side can ask "would this be accepted?"
/// without touching the swarm.
pub fn install_checked(
    cfg: &Config,
    breed: &str,
    bundle: &[u8],
    dry_run: bool,
    probe_ctx: impl FnOnce(&str) -> serde_json::Value,
) -> Result<(BreedInfo, Vec<crate::breed_lint::Finding>)> {
    validate_name(breed)?;
    if breed == cfg.provisioner.default_breed || breed == DEFAULT_BREED {
        return Err(Error::BadBundle(format!(
            "'{breed}' is the built-in breed backed by provisioner.template_dir; \
             push under a different name"
        )));
    }
    let root = breeds_root(cfg)?;
    std::fs::create_dir_all(root)?;

    // Unpack somewhere disposable and inspect it there, so a rejected
    // bundle never touches the directory tenants render from.
    let staging = root.join(format!("{STAGING_PREFIX}{breed}-{}", uuid::Uuid::new_v4().simple()));
    let checked = (|| -> Result<(Vec<crate::breed_lint::Finding>, BTreeMap<String, String>)> {
        std::fs::create_dir_all(&staging)?;
        unpack(bundle, &staging, cfg.provisioner.max_bundle_bytes)?;
        validate_tree(&staging)?;

        let sources = read_sources(&staging)?;
        let ctx = probe_ctx(breed);
        let tpl = sources.get("config.toml.hbs").cloned().unwrap_or_default();
        let hb = handlebars::Handlebars::new();
        let rendered = hb.render_template(&tpl, &ctx).map_err(|e| {
            Error::BadBundle(format!("config.toml.hbs failed to render: {e}"))
        })?;

        let tpl_cfg = &cfg.zeroclaw_template;
        let findings = crate::breed_lint::lint(&crate::breed_lint::Input {
            sources: &sources,
            rendered_config: &rendered,
            swarm_provider: &tpl_cfg.default_provider,
            swarm_api_url: tpl_cfg.api_url.as_deref().unwrap_or_default(),
        });
        Ok((findings, sources))
    })();

    let (findings, _sources) = match checked {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    let errors: Vec<&crate::breed_lint::Finding> = findings
        .iter()
        .filter(|f| f.level == crate::breed_lint::Level::Error)
        .collect();
    if !errors.is_empty() {
        let detail = errors
            .iter()
            .map(|f| format!("[{}] {}", f.rule, f.message))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::remove_dir_all(&staging);
        return Err(Error::BadBundle(format!(
            "品种校验未通过（{} 项）：\n{detail}",
            errors.len()
        )));
    }

    let warnings: Vec<crate::breed_lint::Finding> = findings
        .into_iter()
        .filter(|f| f.level == crate::breed_lint::Level::Warning)
        .collect();

    if dry_run {
        let files = manifest(&staging)?.len();
        let digest = digest_of(&manifest(&staging)?);
        let _ = std::fs::remove_dir_all(&staging);
        return Ok((
            BreedInfo {
                name: breed.to_string(),
                path: root.join(breed).display().to_string(),
                files,
                digest,
                builtin: false,
                tenants: 0,
            },
            warnings,
        ));
    }

    let live = root.join(breed);
    let trash = root.join(format!("{TRASH_PREFIX}{breed}-{}", uuid::Uuid::new_v4().simple()));
    let had_previous = live.exists();
    if had_previous {
        std::fs::rename(&live, &trash)?;
    }
    if let Err(e) = std::fs::rename(&staging, &live) {
        if had_previous {
            let _ = std::fs::rename(&trash, &live);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e.into());
    }
    let _ = std::fs::remove_dir_all(&trash);

    let info = describe(cfg, breed, 0)?.ok_or_else(|| {
        Error::Other(format!("breed '{breed}' unreadable immediately after install"))
    })?;
    Ok((info, warnings))
}

pub fn install(cfg: &Config, breed: &str, bundle: &[u8]) -> Result<BreedInfo> {
    validate_name(breed)?;
    if breed == cfg.provisioner.default_breed || breed == DEFAULT_BREED {
        return Err(Error::BadBundle(format!(
            "'{breed}' is the built-in breed backed by provisioner.template_dir; \
             push under a different name"
        )));
    }
    let root = breeds_root(cfg)?;
    std::fs::create_dir_all(root)?;

    let staging = root.join(format!("{STAGING_PREFIX}{breed}-{}", uuid::Uuid::new_v4().simple()));
    // Any early return from here on must not leave the staging tree
    // behind, hence the explicit cleanup on each error path.
    let result = (|| -> Result<()> {
        std::fs::create_dir_all(&staging)?;
        unpack(bundle, &staging, cfg.provisioner.max_bundle_bytes)?;
        validate_tree(&staging)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    let live = root.join(breed);
    let trash = root.join(format!("{TRASH_PREFIX}{breed}-{}", uuid::Uuid::new_v4().simple()));
    let had_previous = live.exists();
    if had_previous {
        std::fs::rename(&live, &trash)?;
    }
    if let Err(e) = std::fs::rename(&staging, &live) {
        // Put the old tree back before surfacing the failure — a breed
        // that vanished is worse than a push that didn't land.
        if had_previous {
            let _ = std::fs::rename(&trash, &live);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e.into());
    }
    let _ = std::fs::remove_dir_all(&trash);

    describe(cfg, breed, 0)?.ok_or_else(|| {
        Error::Other(format!("breed '{breed}' unreadable immediately after install"))
    })
}

/// Delete a breed's template directory. The caller must have already
/// established that no tenant renders from it.
pub fn remove(cfg: &Config, breed: &str) -> Result<()> {
    validate_name(breed)?;
    if breed == cfg.provisioner.default_breed || breed == DEFAULT_BREED {
        return Err(Error::BadBundle(
            "refusing to delete the built-in breed".into(),
        ));
    }
    let root = breeds_root(cfg)?;
    let live = root.join(breed);
    if !live.is_dir() {
        return Err(Error::UnknownBreed(breed.to_string()));
    }
    std::fs::remove_dir_all(&live)?;
    Ok(())
}

/// Extract a tar (auto-detecting gzip) into `dest`.
///
/// Written by hand rather than `Archive::unpack` so that each guard is
/// explicit and testable: only regular files and directories, only
/// relative paths that stay inside `dest`, and a hard ceiling on total
/// decompressed bytes so a small bundle can't expand into the disk.
fn unpack(bundle: &[u8], dest: &Path, max_bytes: usize) -> Result<()> {
    if bundle.is_empty() {
        return Err(Error::BadBundle("bundle is empty".into()));
    }
    let gzipped = bundle.starts_with(&[0x1f, 0x8b]);
    let reader: Box<dyn Read> = if gzipped {
        Box::new(flate2::read::GzDecoder::new(bundle))
    } else {
        Box::new(bundle)
    };
    let mut archive = tar::Archive::new(reader);
    // Ownership in the tar is meaningless here: files land as whatever
    // ClawOps runs as, and the provisioner chowns per tenant on render.
    archive.set_preserve_permissions(false);
    archive.set_unpack_xattrs(false);

    let mut total: usize = 0;
    let mut count: usize = 0;
    let entries = archive
        .entries()
        .map_err(|e| Error::BadBundle(format!("not a readable tar: {e}")))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| Error::BadBundle(format!("corrupt tar entry: {e}")))?;
        count += 1;
        if count > MAX_ENTRIES {
            return Err(Error::BadBundle(format!(
                "bundle has more than {MAX_ENTRIES} entries"
            )));
        }

        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            // Links are the classic way out of an extraction root, and
            // no legitimate template tree needs one.
            return Err(Error::BadBundle(format!(
                "bundle contains a link ({}); links are not allowed",
                entry.path().map(|p| p.display().to_string()).unwrap_or_default()
            )));
        }
        if !kind.is_file() && !kind.is_dir() {
            // pax/global headers and the like: skip, don't fail.
            continue;
        }

        let raw = entry
            .path()
            .map_err(|e| Error::BadBundle(format!("undecodable path in tar: {e}")))?
            .into_owned();
        let Some(rel) = safe_relative(&raw) else {
            return Err(Error::BadBundle(format!(
                "unsafe path in bundle: {}",
                raw.display()
            )));
        };
        if rel.as_os_str().is_empty() {
            continue; // the "./" entry itself
        }
        let out = dest.join(&rel);

        if kind.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }

        let size = entry.header().size().unwrap_or(0) as usize;
        total = total.saturating_add(size);
        if total > max_bytes {
            return Err(Error::BadBundle(format!(
                "bundle exceeds max_bundle_bytes ({max_bytes})"
            )));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Read through a limited reader rather than trusting the header
        // size: a tar can claim one length and carry another.
        let mut buf = Vec::with_capacity(size.min(1 << 20));
        let remaining = max_bytes.saturating_sub(total.saturating_sub(size));
        entry
            .by_ref()
            .take(remaining as u64 + 1)
            .read_to_end(&mut buf)?;
        if buf.len() > remaining {
            return Err(Error::BadBundle(format!(
                "bundle exceeds max_bundle_bytes ({max_bytes})"
            )));
        }
        std::fs::write(&out, &buf)?;
    }

    if count == 0 {
        return Err(Error::BadBundle("bundle contains no entries".into()));
    }
    Ok(())
}

/// Reduce a tar path to a relative path guaranteed to stay under the
/// extraction root, or `None` if it can't be. Rejects absolute paths,
/// Windows prefixes and any `..`; tolerates the leading `./` that
/// `tar -C dir -cf - .` produces.
fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// Everything that must hold before a staged tree is allowed to go live.
fn validate_tree(dir: &Path) -> Result<()> {
    for required in REQUIRED_FILES {
        if !dir.join(required).is_file() {
            return Err(Error::BadBundle(format!(
                "bundle is missing required file '{required}'"
            )));
        }
    }

    // Compile every template. handlebars only reports the first error in
    // a file, but one real filename and line beats "render failed" three
    // hours later on a tenant's first message.
    let hb = handlebars::Handlebars::new();
    let m = manifest(dir)?;
    for rel in m.keys() {
        if !rel.ends_with(".hbs") {
            continue;
        }
        let text = std::fs::read_to_string(dir.join(rel)).map_err(|e| {
            Error::BadBundle(format!("{rel} is not valid UTF-8 text: {e}"))
        })?;
        if let Err(e) = hb.render_template(&text, &serde_json::json!({})) {
            return Err(Error::BadBundle(format!("{rel} failed to compile: {e}")));
        }
    }
    if m.is_empty() {
        return Err(Error::BadBundle("bundle unpacked to an empty tree".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a tar.gz from `(path, contents)` pairs, the way
    /// `tar -C dir -czf - .` would.
    fn bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, *data).unwrap();
        }
        let raw = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    /// A tar carrying a `..` path. `tar::Builder` refuses to write one,
    /// so the name goes straight into the header bytes — which is what a
    /// hostile bundle would do anyway.
    fn traversal_bundle() -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let data = b"nope\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        let name = b"../../../../tmp/pwned.hbs";
        header.as_old_mut().name[..name.len()].copy_from_slice(name);
        header.set_cksum();
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
        // Two zero blocks terminate the archive.
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    /// A bundle whose single entry is a symlink escaping the root.
    fn link_bundle() -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        tar.append_link(&mut header, "config.toml.hbs", "/etc/passwd")
            .unwrap();
        tar.into_inner().unwrap()
    }

    fn cfg_with(dir: &Path, breeds: &Path) -> Config {
        let text = format!(
            r#"
[server]
host = "127.0.0.1"
port = 8088
[database]
url = "sqlite://:memory:"
[zeroclaw]
binary = "/usr/local/bin/zeroclaw"
home_base = "{home}"
port_range_start = 43000
port_range_end = 44000
[provisioner]
backend = "mock"
template_dir = "{tpl}"
breeds_dir = "{breeds}"
[zeroclaw_template]
default_provider = "deepseek"
default_model = "deepseek-v4-pro"
"#,
            home = dir.join("home").display(),
            tpl = dir.join("builtin").display(),
            breeds = breeds.display(),
        );
        toml::from_str(&text).unwrap()
    }

    fn ok_bundle() -> Vec<u8> {
        bundle(&[
            ("./config.toml.hbs", b"default_model = \"{{llm.default_model}}\"\n"),
            ("./IDENTITY.md.hbs", b"# {{display_name}}\n"),
            ("./skills/demo/SKILL.md.hbs", b"do the thing\n"),
        ])
    }

    #[test]
    fn install_then_describe_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let cfg = cfg_with(tmp.path(), &breeds);

        let info = install(&cfg, "shangji", &ok_bundle()).unwrap();
        assert_eq!(info.files, 3);
        assert!(!info.builtin);
        assert!(breeds.join("shangji/skills/demo/SKILL.md.hbs").is_file());

        // The provisioner must now resolve this breed to that directory.
        assert_eq!(
            cfg.provisioner.breed_dir("shangji").unwrap(),
            breeds.join("shangji")
        );
    }

    #[test]
    fn digest_tracks_content_not_timing() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let cfg = cfg_with(tmp.path(), &breeds);

        let first = install(&cfg, "shangji", &ok_bundle()).unwrap().digest;
        let same = install(&cfg, "shangji", &ok_bundle()).unwrap().digest;
        assert_eq!(first, same, "identical trees must produce identical digests");

        let changed = install(
            &cfg,
            "shangji",
            &bundle(&[
                ("./config.toml.hbs", b"default_model = \"{{llm.default_model}}\"\n"),
                ("./IDENTITY.md.hbs", b"# {{display_name}} (v2)\n"),
                ("./skills/demo/SKILL.md.hbs", b"do the thing\n"),
            ]),
        )
        .unwrap()
        .digest;
        assert_ne!(first, changed);
    }

    #[test]
    fn reinstall_drops_files_the_new_bundle_omits() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let cfg = cfg_with(tmp.path(), &breeds);

        install(&cfg, "shangji", &ok_bundle()).unwrap();
        assert!(breeds.join("shangji/skills/demo/SKILL.md.hbs").exists());

        install(
            &cfg,
            "shangji",
            &bundle(&[("./config.toml.hbs", b"x = 1\n")]),
        )
        .unwrap();
        // A skill removed upstream has to disappear here, not linger and
        // keep being rendered into every tenant's workspace.
        assert!(!breeds.join("shangji/skills/demo/SKILL.md.hbs").exists());
    }

    #[test]
    fn rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let cfg = cfg_with(tmp.path(), &breeds);

        let err = install(&cfg, "shangji", &traversal_bundle()).unwrap_err();
        assert!(matches!(err, Error::BadBundle(_)), "got {err:?}");
        assert!(!breeds.join("shangji").exists(), "nothing may be left behind");
    }

    #[test]
    fn rejects_links() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let cfg = cfg_with(tmp.path(), &breeds);

        let err = install(&cfg, "shangji", &link_bundle()).unwrap_err();
        assert!(matches!(err, Error::BadBundle(_)), "got {err:?}");
    }

    #[test]
    fn rejects_oversize_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let mut cfg = cfg_with(tmp.path(), &breeds);
        cfg.provisioner.max_bundle_bytes = 1024;

        let big = vec![b'x'; 4096];
        let err = install(
            &cfg,
            "shangji",
            &bundle(&[("./config.toml.hbs", b"x = 1\n"), ("./big.txt", &big)]),
        )
        .unwrap_err();
        assert!(matches!(err, Error::BadBundle(_)), "got {err:?}");
    }

    #[test]
    fn rejects_bundle_without_config_template() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let cfg = cfg_with(tmp.path(), &breeds);

        let err = install(&cfg, "shangji", &bundle(&[("./IDENTITY.md.hbs", b"hi\n")]))
            .unwrap_err();
        assert!(
            err.to_string().contains("config.toml.hbs"),
            "error should name the missing file: {err}"
        );
    }

    #[test]
    fn rejects_uncompilable_template_and_keeps_previous_live() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let cfg = cfg_with(tmp.path(), &breeds);

        let good = install(&cfg, "shangji", &ok_bundle()).unwrap();

        let broken = bundle(&[
            ("./config.toml.hbs", b"x = 1\n"),
            ("./IDENTITY.md.hbs", b"{{#if unclosed}}oops\n"),
        ]);
        let err = install(&cfg, "shangji", &broken).unwrap_err();
        assert!(err.to_string().contains("IDENTITY.md.hbs"), "got {err}");

        // The rejected push must not have disturbed what is live.
        let counts = BTreeMap::new();
        let still = describe(&cfg, "shangji", 0).unwrap().unwrap();
        assert_eq!(still.digest, good.digest);
        assert!(list(&cfg, &counts).unwrap().iter().any(|b| b.name == "shangji"));
    }

    #[test]
    fn refuses_to_overwrite_the_builtin_breed() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let cfg = cfg_with(tmp.path(), &breeds);

        let err = install(&cfg, "default", &ok_bundle()).unwrap_err();
        assert!(matches!(err, Error::BadBundle(_)), "got {err:?}");
        assert!(remove(&cfg, "default").is_err());
    }

    #[test]
    fn rejects_bad_names() {
        for bad in ["", "Shangji", "../etc", "a b", "-lead", "x".repeat(65).as_str()] {
            assert!(validate_name(bad).is_err(), "'{bad}' should be rejected");
        }
        for good in ["shangji", "huairou-v2", "breed_1"] {
            validate_name(good).unwrap();
        }
    }

    #[test]
    fn breeds_disabled_without_breeds_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = cfg_with(tmp.path(), &tmp.path().join("breeds"));
        cfg.provisioner.breeds_dir = None;

        assert!(matches!(
            install(&cfg, "shangji", &ok_bundle()).unwrap_err(),
            Error::BreedsDisabled
        ));
        // The built-in breed keeps resolving — single-breed mode is the
        // pre-existing behaviour and must stay intact.
        assert_eq!(
            cfg.provisioner.breed_dir("default").unwrap(),
            cfg.provisioner.template_dir
        );
        assert!(cfg.provisioner.breed_dir("shangji").is_none());
    }

    #[test]
    fn list_reports_builtin_plus_uploaded() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let cfg = cfg_with(tmp.path(), &breeds);
        std::fs::create_dir_all(&cfg.provisioner.template_dir).unwrap();
        std::fs::write(cfg.provisioner.template_dir.join("config.toml.hbs"), "x = 1\n").unwrap();

        install(&cfg, "shangji", &ok_bundle()).unwrap();

        let counts: BTreeMap<String, i64> =
            [("default".to_string(), 3i64), ("shangji".to_string(), 7i64)].into();
        let listed = list(&cfg, &counts).unwrap();
        let names: Vec<&str> = listed.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, vec!["default", "shangji"]);
        assert!(listed[0].builtin);
        assert_eq!(listed[0].tenants, 3);
        assert_eq!(listed[1].tenants, 7);
    }

    #[test]
    fn staging_and_trash_dirs_never_show_up_as_breeds() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let cfg = cfg_with(tmp.path(), &breeds);
        std::fs::create_dir_all(&cfg.provisioner.template_dir).unwrap();
        std::fs::create_dir_all(breeds.join(format!("{STAGING_PREFIX}leftover"))).unwrap();
        std::fs::create_dir_all(breeds.join(format!("{TRASH_PREFIX}leftover"))).unwrap();

        let listed = list(&cfg, &BTreeMap::new()).unwrap();
        assert_eq!(listed.len(), 1, "only the builtin breed: {listed:?}");
    }

    #[test]
    fn remove_deletes_only_the_named_breed() {
        let tmp = tempfile::tempdir().unwrap();
        let breeds = tmp.path().join("breeds");
        let cfg = cfg_with(tmp.path(), &breeds);

        install(&cfg, "shangji", &ok_bundle()).unwrap();
        install(&cfg, "huairou", &ok_bundle()).unwrap();
        remove(&cfg, "shangji").unwrap();

        assert!(!breeds.join("shangji").exists());
        assert!(breeds.join("huairou").exists());
        assert!(matches!(
            remove(&cfg, "shangji").unwrap_err(),
            Error::UnknownBreed(_)
        ));
    }
}
