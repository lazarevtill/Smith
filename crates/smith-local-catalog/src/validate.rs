use crate::*;
use std::{collections::BTreeSet, path::Path};

pub(crate) fn text(value: &str, max: usize) -> Result<()> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        Err(Error::InvalidRecord)
    } else {
        Ok(())
    }
}
pub(crate) fn hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn relative(name: &str) -> Result<()> {
    text(name, 512)?;
    if name.contains(['\\', ':'])
        || name.split('/').count() > 16
        || name
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(Error::InvalidSelection);
    }
    Ok(())
}
/// Pure lexical checks permit foreign-platform records to be browsed. Native
/// file opening separately refuses unsupported path/platform combinations.
pub(crate) fn root(path: &Path) -> Result<()> {
    let value = path.to_str().ok_or(Error::InvalidSelection)?;
    text(value, 4096)?;
    let parts = if let Some(rest) = value.strip_prefix('/') {
        if value == "/" {
            return Ok(());
        }
        if value.contains('\\') {
            return Err(Error::InvalidSelection);
        }
        rest.split('/').collect::<Vec<_>>()
    } else if value.as_bytes().get(1) == Some(&b':')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && value.as_bytes().get(2) == Some(&b'\\')
    {
        if value.contains('/') {
            return Err(Error::InvalidSelection);
        }
        if value.len() == 3 {
            return Ok(());
        }
        value[3..].split('\\').collect::<Vec<_>>()
    } else {
        return Err(Error::InvalidSelection);
    };
    if parts.len() > 128
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return Err(Error::InvalidSelection);
    }
    Ok(())
}
fn source(s: &Source) -> Result<()> {
    text(&s.label, 256)?;
    text(&s.revision, 256)
}
fn bounded(value: Option<u32>, max: u32) -> bool {
    value.is_none_or(|n| n > 0 && n <= max)
}
pub(crate) fn description(d: &ModelDescription) -> Result<()> {
    text(&d.architecture, 128)?;
    if d.declared_capabilities.len() > 5
        || d.declared_capabilities
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != d.declared_capabilities.len()
        || !bounded(d.declared_context_limit, 1_048_576)
    {
        return Err(Error::InvalidRecord);
    }
    let (context, output) = match (&d.format, &d.hints) {
        (ModelFormat::Gguf, AdapterHints::LlamaGguf(h)) => {
            if !bounded(h.threads.map(u32::from), 512)
                || !bounded(h.prefill_threads.map(u32::from), 512)
                || !bounded(h.batch_tokens, 1_048_576)
                || !bounded(h.microbatch_tokens, 1_048_576)
                || h.gpu_layers.is_some_and(|n| n > 4096)
                || matches!((h.microbatch_tokens,h.batch_tokens),(Some(a),Some(b)) if a>b)
                || matches!((h.batch_tokens,h.context_tokens),(Some(a),Some(b)) if a>b)
            {
                return Err(Error::InvalidRecord);
            }
            (h.context_tokens, h.output_tokens)
        }
        (ModelFormat::Mlx, AdapterHints::MlxSwift(h)) => {
            if h.temperature_milli.is_some_and(|n| n > 2000)
                || !bounded(h.top_p_milli.map(u32::from), 1000)
                || h.seed.is_some_and(|n| n > MAX_REVISION)
            {
                return Err(Error::InvalidRecord);
            }
            (h.context_tokens, h.output_tokens)
        }
        _ => return Err(Error::InvalidRecord),
    };
    if !bounded(context, 1_048_576)
        || !bounded(output, 32768)
        || matches!((context,d.declared_context_limit),(Some(a),Some(b)) if a>b)
        || matches!((output,context.or(d.declared_context_limit)),(Some(a),Some(b)) if a>b)
    {
        return Err(Error::InvalidRecord);
    }
    // The current MLX adapter's generation budget is narrower than GGUF.
    if d.format == ModelFormat::Mlx && output.is_some_and(|n| n > 2048) {
        return Err(Error::InvalidRecord);
    }
    Ok(())
}
pub(crate) fn role_cap(role: FileRole) -> u64 {
    match role {
        FileRole::Executable => 512 * 1024 * 1024,
        FileRole::RuntimeResource => 1024 * 1024 * 1024,
        FileRole::Weights => MAX_SELECTED_BYTES,
        FileRole::ModelMetadata => 64 * 1024 * 1024,
    }
}
fn layout(
    format: ModelFormat,
    runtime: &[(String, FileRole)],
    model: &[(String, FileRole)],
) -> Result<()> {
    if runtime.is_empty()
        || model.is_empty()
        || runtime.len() + model.len() > MAX_FILES
        || runtime
            .iter()
            .filter(|(_, r)| *r == FileRole::Executable)
            .count()
            != 1
        || runtime
            .iter()
            .any(|(_, r)| !matches!(r, FileRole::Executable | FileRole::RuntimeResource))
        || model
            .iter()
            .any(|(_, r)| !matches!(r, FileRole::Weights | FileRole::ModelMetadata))
    {
        return Err(Error::InvalidSelection);
    }
    match format {
        ModelFormat::Gguf if model.len() == 1 && model[0].1 == FileRole::Weights => {}
        ModelFormat::Mlx
            if model
                .iter()
                .any(|(n, r)| *r == FileRole::Weights && n.ends_with(".safetensors"))
                && model
                    .iter()
                    .all(|(n, r)| *r != FileRole::Weights || n.ends_with(".safetensors"))
                && ["config.json", "tokenizer.json", "tokenizer_config.json"]
                    .iter()
                    .all(|required| {
                        model
                            .iter()
                            .any(|(n, r)| n.as_str() == *required && *r == FileRole::ModelMetadata)
                    }) => {}
        _ => return Err(Error::InvalidSelection),
    }
    Ok(())
}
pub(crate) fn request(r: &CaptureRequest) -> Result<()> {
    description(&r.description)?;
    let mut paths = BTreeSet::new();
    for p in [&r.runtime, &r.model] {
        root(&p.root)?;
        source(&p.source)?;
        if p.files.len() > MAX_FILES {
            return Err(Error::LimitExceeded);
        }
        for f in &p.files {
            relative(&f.name)?;
            if !paths.insert(p.root.join(&f.name)) {
                return Err(Error::InvalidSelection);
            }
        }
    }
    layout(
        r.description.format,
        &r.runtime
            .files
            .iter()
            .map(|f| (f.name.clone(), f.role))
            .collect::<Vec<_>>(),
        &r.model
            .files
            .iter()
            .map(|f| (f.name.clone(), f.role))
            .collect::<Vec<_>>(),
    )
}
pub(crate) fn manifest(m: &ManifestRecord) -> Result<()> {
    description(&m.description)?;
    let mut total = 0u64;
    let mut paths = BTreeSet::new();
    for p in [&m.runtime, &m.model] {
        root(&p.root)?;
        source(&p.source)?;
        if p.files.len() > MAX_FILES {
            return Err(Error::LimitExceeded);
        }
        let mut previous: Option<&str> = None;
        for f in &p.files {
            relative(&f.name)?;
            if previous.is_some_and(|prev| prev >= f.name.as_str())
                || !paths.insert(p.root.join(&f.name))
                || !hash(&f.sha256)
                || f.bytes == 0
                || f.bytes > role_cap(f.role)
            {
                return Err(Error::InvalidRecord);
            }
            previous = Some(&f.name);
            total = total.checked_add(f.bytes).ok_or(Error::LimitExceeded)?;
        }
    }
    if total > MAX_SELECTED_BYTES {
        return Err(Error::LimitExceeded);
    }
    layout(
        m.description.format,
        &m.runtime
            .files
            .iter()
            .map(|f| (f.name.clone(), f.role))
            .collect::<Vec<_>>(),
        &m.model
            .files
            .iter()
            .map(|f| (f.name.clone(), f.role))
            .collect::<Vec<_>>(),
    )?;
    if !hash(&m.sha256) || m.sha256 != manifest_hash(m)? {
        return Err(Error::InvalidRecord);
    }
    Ok(())
}
pub(crate) fn manifest_hash(m: &ManifestRecord) -> Result<String> {
    digest(&(
        "smith-local-catalog/selected-files/v1",
        &m.description,
        &m.runtime,
        &m.model,
    ))
}
pub(crate) fn record(r: &CatalogRecord) -> Result<()> {
    if r.format_version != 1 {
        return Err(Error::UnsupportedVersion);
    }
    if r.revision > MAX_REVISION || r.entries.len() > MAX_ENTRIES {
        return Err(Error::LimitExceeded);
    }
    let mut ids = BTreeSet::new();
    for e in &r.entries {
        text(&e.label, 256)?;
        let birth =
            e.id.strip_prefix("local-")
                .filter(|s| s.len() == 16)
                .and_then(|s| u64::from_str_radix(s, 16).ok())
                .ok_or(Error::InvalidRecord)?;
        if birth == 0
            || birth > e.revision
            || e.id != format!("local-{birth:016x}")
            || e.revision > r.revision
            || !ids.insert(&e.id)
        {
            return Err(Error::InvalidRecord);
        }
        manifest(&e.manifest)?;
    }
    if r.selected_for_review
        .as_ref()
        .is_some_and(|id| !ids.contains(id))
    {
        return Err(Error::InvalidRecord);
    }
    if r.entries
        .iter()
        .any(|e| e.trashed && r.selected_for_review.as_deref() == Some(e.id.as_str()))
    {
        return Err(Error::InvalidRecord);
    }
    Ok(())
}
pub(crate) fn context(c: &PreparationContext) -> Result<()> {
    text(&c.session_id, 128)?;
    text(&c.inventory_generation, 128)?;
    text(&c.config_revision, 128)?;
    Ok(())
}
