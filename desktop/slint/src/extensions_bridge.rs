//! Reusable instruction definitions are data, never capabilities or execution.
//! Every action carries the session and exact record that the user inspected.
use crate::bridge::{encode_component, Bridge, Result, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionRef {
    pub id: String,
    pub revision: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionSelection {
    #[default]
    Enabled,
    Explicit {
        references: Vec<InstructionRef>,
    },
}
impl InstructionSelection {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }
    pub fn validate(&self) -> Result<()> {
        if let Self::Explicit { references } = self {
            let mut seen = BTreeSet::new();
            if references.len() > 12
                || references
                    .iter()
                    .any(|r| !identity(&r.id) || r.revision == 0 || !seen.insert(&r.id))
            {
                return Err(
                    "Select at most twelve distinct, current instruction definitions.".into(),
                );
            }
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionKind {
    Skill,
    Method,
    Plugin,
    Recipe,
}
impl ExtensionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Skill => "Skill",
            Self::Method => "Method",
            Self::Plugin => "Plugin",
            Self::Recipe => "Recipe",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionDefinition {
    Skill {
        name: String,
        description: String,
        instructions: String,
        #[serde(default)]
        category: String,
    },
    Method {
        name: String,
        description: String,
        instructions: String,
    },
    Plugin {
        manifest_id: String,
        name: String,
        description: String,
        #[serde(default)]
        version: String,
        #[serde(default)]
        authors: Vec<String>,
        #[serde(default)]
        homepage: Option<String>,
        #[serde(default)]
        license: Option<String>,
        #[serde(default)]
        instructions: Option<String>,
        #[serde(default)]
        instructions_file: Option<String>,
        #[serde(default)]
        tools: Vec<String>,
    },
    Recipe {
        name: String,
        description: String,
        command: String,
        parameters: Value,
    },
}
impl ExtensionDefinition {
    pub fn kind(&self) -> ExtensionKind {
        match self {
            Self::Skill { .. } => ExtensionKind::Skill,
            Self::Method { .. } => ExtensionKind::Method,
            Self::Plugin { .. } => ExtensionKind::Plugin,
            Self::Recipe { .. } => ExtensionKind::Recipe,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Self::Skill { name, .. }
            | Self::Method { name, .. }
            | Self::Plugin { name, .. }
            | Self::Recipe { name, .. } => name,
        }
    }
    pub fn description(&self) -> &str {
        match self {
            Self::Skill { description, .. }
            | Self::Method { description, .. }
            | Self::Plugin { description, .. }
            | Self::Recipe { description, .. } => description,
        }
    }
    pub fn instructions(&self) -> Option<&str> {
        match self {
            Self::Skill { instructions, .. } | Self::Method { instructions, .. } => {
                Some(instructions)
            }
            Self::Plugin { instructions, .. } => instructions.as_deref(),
            Self::Recipe { .. } => None,
        }
    }
    pub fn validate(&self) -> Result<()> {
        if self.name().trim().is_empty()
            || self.name().len() > 80
            || self.name().chars().any(char::is_control)
            || self.description().len() > 2048
            || self.instructions().is_some_and(|v| v.len() > 8192)
            || serde_json::to_vec(self)
                .map_err(|_| "Invalid extension data.")?
                .len()
                > 32768
        {
            return Err("Use a name of 1–80 bytes, description up to 2048 bytes and instructions up to 8192 bytes.".into());
        }
        match self {
            Self::Skill{category,..} if category.len()>128 => return Err("Category is too long.".into()),
            Self::Plugin{manifest_id,version,authors,homepage,license,instructions,instructions_file,tools,..} => {
                if manifest_id.trim().is_empty() || manifest_id.len()>128 || version.len()>128 || authors.len()>16 || tools.len()>64 || authors.iter().chain(tools).any(|v|v.trim().is_empty()||v.len()>128||v.chars().any(char::is_control)) || homepage.as_ref().is_some_and(|v|v.len()>2048) || license.as_ref().is_some_and(|v|v.len()>256) || instructions_file.as_ref().is_some_and(|v|v.len()>512||instructions.is_none()) { return Err("Plugin metadata exceeds its bound or references instructions that were not supplied inline.".into()); }
            }
            Self::Recipe{command,parameters,..} if command.len()>8192 || !parameters.is_object() || serde_json::to_vec(parameters).map_err(|_|"Invalid recipe parameters.")?.len()>8192 => return Err("Use a bounded command and a JSON object for recipe parameters. Recipe execution is unavailable.".into()),
            _=>{}
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionSummary {
    pub id: String,
    pub revision: u64,
    pub kind: ExtensionKind,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub deleted: bool,
    pub instruction_bytes: Option<usize>,
    pub execution_status: String,
    pub provenance: Value,
    pub created_at: i64,
    pub updated_at: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionRecord {
    #[serde(flatten)]
    pub summary: ExtensionSummary,
    pub definition: ExtensionDefinition,
}
#[derive(Clone)]
pub struct ExtensionSelection {
    session: String,
    summary: ExtensionSummary,
    used: Arc<AtomicBool>,
}
#[derive(Clone, Default)]
pub struct ExtensionCatalog {
    pub rows: Vec<ExtensionSummary>,
    selections: Vec<ExtensionSelection>,
}
impl ExtensionCatalog {
    pub fn select(&self, row: &ExtensionSummary) -> Result<ExtensionSelection> {
        self.selections
            .iter()
            .find(|s| s.summary == *row)
            .cloned()
            .ok_or_else(|| "Refresh the extension list before selecting this row.".into())
    }
}
#[derive(Clone)]
pub struct ExtensionReview {
    session: String,
    record: ExtensionRecord,
    used: Arc<AtomicBool>,
}
impl ExtensionReview {
    pub fn record(&self) -> &ExtensionRecord {
        &self.record
    }
}
#[derive(Clone)]
pub struct ExtensionEditor {
    session: String,
    used: Arc<AtomicBool>,
}
#[derive(Clone)]
pub struct ExtensionEdit {
    session: String,
    old: Option<ExtensionRecord>,
    definition: ExtensionDefinition,
    enabled: bool,
    used: Arc<AtomicBool>,
}
impl ExtensionEditor {
    pub fn edit(
        &self,
        old: Option<&ExtensionReview>,
        definition: ExtensionDefinition,
        enabled: bool,
    ) -> Result<ExtensionEdit> {
        definition.validate()?;
        if old.is_some_and(|v| {
            v.session != self.session
                || v.record.summary.deleted
                || v.record.summary.kind != definition.kind()
        }) {
            return Err(
                "Refresh this live extension before editing it; its kind cannot change.".into(),
            );
        }
        Ok(ExtensionEdit {
            session: self.session.clone(),
            old: old.map(|v| v.record.clone()),
            definition,
            enabled,
            used: old
                .map(|v| v.used.clone())
                .unwrap_or_else(|| self.used.clone()),
        })
    }
}
fn identity(id: &str) -> bool {
    id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}
fn consume(used: &AtomicBool) -> Result<()> {
    if used.swap(true, Ordering::SeqCst) {
        Err("This change was already submitted. Refresh before another explicit action.".into())
    } else {
        Ok(())
    }
}
fn valid_summary(r: &ExtensionSummary) -> Result<()> {
    if !identity(&r.id)
        || r.revision == 0
        || r.name.trim().is_empty()
        || r.name.len() > 80
        || r.description.len() > 2048
        || r.instruction_bytes.is_some_and(|n| n > 8192)
        || r.execution_status
            != if r.kind == ExtensionKind::Recipe {
                "unsupported_wsl2_recipe"
            } else {
                "instruction_data"
            }
    {
        return Err("Invalid extension summary.".into());
    }
    Ok(())
}
fn valid_record(r: &ExtensionRecord) -> Result<()> {
    valid_summary(&r.summary)?;
    r.definition.validate()?;
    if r.summary.kind != r.definition.kind()
        || r.summary.name != r.definition.name()
        || r.summary.description != r.definition.description()
        || r.summary.instruction_bytes != r.definition.instructions().map(str::len)
    {
        return Err("Extension definition differs from its summary.".into());
    }
    Ok(())
}
impl<T: Transport> Bridge<T> {
    fn extension_session(&self) -> Result<String> {
        if !self.view.available {
            return Err("Workspace unavailable.".into());
        }
        self.session
            .clone()
            .ok_or_else(|| "Workspace unavailable.".into())
    }
    fn extension_check(&mut self, session: &str) -> Result<()> {
        if self.session.as_deref() != Some(session) {
            return Err("This extension review belongs to an earlier workspace.".into());
        }
        self.check_session()
    }
    pub fn extension_editor(&self) -> Result<ExtensionEditor> {
        Ok(ExtensionEditor {
            session: self.extension_session()?,
            used: Arc::new(AtomicBool::new(false)),
        })
    }
    pub fn extension_catalog(&mut self) -> Result<ExtensionCatalog> {
        let session = self.extension_session()?;
        let mut rows = Vec::new();
        let mut after = None::<String>;
        let mut seen = BTreeSet::new();
        for page in 0..8 {
            self.extension_check(&session)?;
            let path = format!(
                "/api/agent/extensions?include_deleted=true&limit=32{}",
                after
                    .as_ref()
                    .map(|a| format!("&after={}", encode_component(a)))
                    .unwrap_or_default()
            );
            let result = self.get(&path)?;
            self.extension_check(&session)?;
            let batch: Vec<ExtensionSummary> = serde_json::from_value(result["extensions"].clone())
                .map_err(|_| "Invalid extension list.")?;
            if batch.len() > 32
                || rows.len() + batch.len() > 256
                || batch.iter().any(|v| !seen.insert(v.id.clone()))
            {
                return Err("Extension list repeated rows or exceeded its bound.".into());
            }
            for row in &batch {
                valid_summary(row)?;
            }
            let last = batch.last().map(|r| r.id.clone());
            rows.extend(batch);
            match result.get("next_after") {
                Some(Value::Null) => break,
                Some(Value::String(next))
                    if page < 7
                        && identity(next)
                        && last.as_ref() == Some(next)
                        && after.as_ref() != Some(next) =>
                {
                    after = Some(next.clone())
                }
                _ => return Err("Invalid extension continuation. Refresh the list.".into()),
            }
        }
        let selections = rows
            .iter()
            .map(|summary| ExtensionSelection {
                session: session.clone(),
                summary: summary.clone(),
                used: Arc::new(AtomicBool::new(false)),
            })
            .collect();
        Ok(ExtensionCatalog { rows, selections })
    }
    pub fn extension_open(&mut self, selection: &ExtensionSelection) -> Result<ExtensionReview> {
        self.extension_check(&selection.session)?;
        let value = self.get(&format!(
            "/api/agent/extensions/{}?include_deleted=true",
            encode_component(&selection.summary.id)
        ))?;
        self.extension_check(&selection.session)?;
        let record: ExtensionRecord = serde_json::from_value(value["extension"].clone())
            .map_err(|_| "Invalid extension definition.")?;
        valid_record(&record)?;
        if record.summary != selection.summary {
            return Err("This extension changed. Refresh before opening the new revision.".into());
        }
        Ok(ExtensionReview {
            session: selection.session.clone(),
            record,
            used: selection.used.clone(),
        })
    }
    pub fn extension_save(&mut self, edit: &ExtensionEdit) -> Result<ExtensionReview> {
        self.extension_check(&edit.session)?;
        consume(&edit.used)?;
        let (method, path, body) = if let Some(old) = &edit.old {
            (
                "PUT",
                format!(
                    "/api/agent/extensions/{}",
                    encode_component(&old.summary.id)
                ),
                json!({"expected_revision":old.summary.revision,"definition":edit.definition,"enabled":edit.enabled}),
            )
        } else {
            (
                "POST",
                "/api/agent/extensions".into(),
                json!({"definition":edit.definition,"enabled":edit.enabled}),
            )
        };
        let result = self
            .transport
            .request(method, &path, Some(&edit.session), Some(body));
        self.extension_check(&edit.session)?;
        let record: ExtensionRecord = serde_json::from_value(result?["extension"].clone())
            .map_err(|_| "Save outcome needs a fresh extension list.")?;
        valid_record(&record)?;
        let revision = edit
            .old
            .as_ref()
            .map(|old| {
                if old.definition == edit.definition && old.summary.enabled == edit.enabled {
                    old.summary.revision
                } else {
                    old.summary.revision.saturating_add(1)
                }
            })
            .unwrap_or(1);
        if record.definition != edit.definition
            || record.summary.enabled != edit.enabled
            || record.summary.deleted
            || record.summary.revision != revision
            || edit.old.as_ref().is_some_and(|old| {
                old.summary.id != record.summary.id
                    || old.summary.created_at != record.summary.created_at
                    || old.summary.provenance != record.summary.provenance
            })
        {
            return Err(
                "Save receipt differs from the reviewed extension. Inspect before another action."
                    .into(),
            );
        }
        Ok(ExtensionReview {
            session: edit.session.clone(),
            record,
            used: Arc::new(AtomicBool::new(false)),
        })
    }
    pub fn extension_set_enabled(
        &mut self,
        review: &ExtensionReview,
        enabled: bool,
    ) -> Result<ExtensionReview> {
        self.extension_change(review, "PATCH", "enabled", Some(enabled))
    }
    pub fn extension_trash(&mut self, review: &ExtensionReview) -> Result<ExtensionReview> {
        self.extension_change(review, "DELETE", "", None)
    }
    pub fn extension_restore(&mut self, review: &ExtensionReview) -> Result<ExtensionReview> {
        self.extension_change(review, "POST", "restore", None)
    }
    fn extension_change(
        &mut self,
        review: &ExtensionReview,
        method: &str,
        suffix: &str,
        enabled: Option<bool>,
    ) -> Result<ExtensionReview> {
        self.extension_check(&review.session)?;
        if (suffix == "restore") != review.record.summary.deleted {
            return Err("Refresh the extension's current live/Trash state.".into());
        }
        consume(&review.used)?;
        let mut body = json!({"expected_revision":review.record.summary.revision});
        if let Some(v) = enabled {
            body["enabled"] = json!(v);
        }
        let path = format!(
            "/api/agent/extensions/{}{}",
            encode_component(&review.record.summary.id),
            if suffix.is_empty() {
                String::new()
            } else {
                format!("/{suffix}")
            }
        );
        let result = self
            .transport
            .request(method, &path, Some(&review.session), Some(body));
        self.extension_check(&review.session)?;
        let value = result?;
        let record: ExtensionRecord = serde_json::from_value(value["extension"].clone())
            .map_err(|_| "Extension change needs a fresh list.")?;
        valid_record(&record)?;
        let old = &review.record;
        let next = old.summary.revision + u64::from(enabled != Some(old.summary.enabled));
        if record.definition != old.definition
            || record.summary.id != old.summary.id
            || record.summary.revision != next
            || record.summary.enabled != enabled.unwrap_or(old.summary.enabled)
            || record.summary.deleted != (method == "DELETE")
            || record.summary.provenance != old.summary.provenance
            || record.summary.created_at != old.summary.created_at
            || value["task_history_preserved"] != true
        {
            return Err("Extension change receipt differs from its exact reviewed action.".into());
        }
        Ok(ExtensionReview {
            session: review.session.clone(),
            record,
            used: Arc::new(AtomicBool::new(false)),
        })
    }
}

/// An import holds one exact request ID before any durable write. An explicit
/// retry reuses this body; a transport error never manufactures a second import.
#[derive(Clone)]
pub struct ExtensionImport {
    session: String,
    path: &'static str,
    body: Value,
    definition: ExtensionDefinition,
    provenance: Value,
    submitted: Arc<AtomicBool>,
}
impl ExtensionImport {
    pub fn definition(&self) -> &ExtensionDefinition {
        &self.definition
    }
    pub fn provenance(&self) -> &Value {
        &self.provenance
    }
    pub fn request_id(&self) -> &str {
        self.body["request_id"].as_str().unwrap_or("")
    }
    pub fn submitted(&self) -> bool {
        self.submitted.load(Ordering::SeqCst)
    }
}
#[derive(Clone)]
pub struct LegacyExtensionRow {
    row_number: u64,
    pub name: String,
    session: String,
    table: String,
}
#[derive(Clone)]
pub struct LegacyExtensionPage {
    pub rows: Vec<LegacyExtensionRow>,
    pub total: u64,
    pub offset: u64,
    session: String,
    table: String,
}
impl LegacyExtensionPage {
    pub fn has_next(&self) -> bool {
        self.offset + (self.rows.len() as u64) < self.total
    }
}
fn digest(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}
impl<T: Transport> Bridge<T> {
    pub fn extension_plugin_review(
        &mut self,
        editor: &ExtensionEditor,
        directory: &std::path::Path,
    ) -> Result<ExtensionImport> {
        self.extension_check(&editor.session)?;
        let path = directory.to_str().ok_or("Choose a UTF-8 directory path.")?;
        if !directory.is_absolute() || path.len() > 4096 || path.chars().any(char::is_control) {
            return Err("Choose one existing absolute plugin directory.".into());
        }
        let result = self.transport.request(
            "POST",
            "/api/agent/extensions/plugin-review",
            Some(&editor.session),
            Some(json!({"directory":path})),
        );
        self.extension_check(&editor.session)?;
        let value = result?;
        if !value["review_id"].as_str().is_some_and(identity)
            || value["expires_in_seconds"] != 300
            || value["trust"] != "explicit_local_selection_not_publisher_verification"
            || value["provenance"]["kind"] != "selected_directory"
            || value["provenance"]["source"]["source_sha256"] != value["source_sha256"]
        {
            return Err("Plugin inspection returned invalid source identity.".into());
        }
        let files = value["provenance"]["source"]["files"]
            .as_array()
            .ok_or("Plugin inspection omitted source files.")?;
        if files.is_empty()
            || files.len() > 2
            || files.iter().any(|f| {
                !digest(&f["sha256"])
                    || f["bytes"].as_u64().is_none()
                    || f["name"]
                        .as_str()
                        .is_none_or(|n| n.is_empty() || n.contains('/') || n.contains('\\'))
            })
        {
            return Err("Plugin source file evidence is invalid.".into());
        }
        let body = json!({"request_id":crate::task_bridge::new_task_request_id()?,"review_id":value["review_id"],"expected_source_sha256":value["source_sha256"]});
        self.extension_import_review(
            &editor.session,
            "/api/agent/extensions/plugin-import",
            body,
            value,
            Some(ExtensionKind::Plugin),
        )
    }
    pub fn extension_legacy_first(
        &mut self,
        editor: &ExtensionEditor,
        kind: ExtensionKind,
    ) -> Result<LegacyExtensionPage> {
        let table = match kind {
            ExtensionKind::Skill => "skills",
            ExtensionKind::Method => "methods",
            ExtensionKind::Recipe => "tool_recipes",
            ExtensionKind::Plugin => {
                return Err("Select a plugin directory instead of an original record.".into())
            }
        };
        self.extension_legacy_page(&editor.session, table, 0)
    }
    pub fn extension_legacy_next(
        &mut self,
        page: &LegacyExtensionPage,
    ) -> Result<LegacyExtensionPage> {
        if !page.has_next() || page.rows.is_empty() {
            return Err("No next original-record page.".into());
        }
        self.extension_legacy_page(
            &page.session,
            &page.table,
            page.offset + page.rows.len() as u64,
        )
    }
    fn extension_legacy_page(
        &mut self,
        session: &str,
        table: &str,
        offset: u64,
    ) -> Result<LegacyExtensionPage> {
        self.extension_check(session)?;
        if offset > 1_000_000 {
            return Err("Original-record page exceeds its bound.".into());
        }
        let value = self.get(&format!(
            "/api/legacy-import/records/{table}?limit=8&offset={offset}"
        ))?;
        self.extension_check(session)?;
        let rows = value["rows"]
            .as_array()
            .ok_or("Invalid original extension records.")?;
        let total = value["total"]
            .as_u64()
            .ok_or("Invalid original extension count.")?;
        if value["table"] != table
            || value["offset"] != offset
            || value["runtime_activation"] != "dormant"
            || rows.len() > 8
            || total < offset + rows.len() as u64
            || (rows.is_empty() && offset < total)
        {
            return Err("Original extension page changed or exceeded its bound.".into());
        }
        let mut prior = None;
        let mut result = Vec::new();
        for row in rows {
            let number = row["row_number"]
                .as_u64()
                .ok_or("Invalid original row number.")?;
            if prior.is_some_and(|p| p >= number) {
                return Err("Original extension rows repeated.".into());
            }
            prior = Some(number);
            let record = &row["record"];
            let name = record["columns"]
                .as_array()
                .and_then(|c| c.iter().position(|v| v == "name"))
                .and_then(|i| record["values"][i].as_object())
                .and_then(|v| {
                    if v.get("type") == Some(&json!("text")) {
                        v.get("value").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .map(|v| v.chars().take(160).collect())
                .unwrap_or_else(|| format!("Original record {number}"));
            result.push(LegacyExtensionRow {
                row_number: number,
                name,
                session: session.into(),
                table: table.into(),
            });
        }
        Ok(LegacyExtensionPage {
            rows: result,
            total,
            offset,
            session: session.into(),
            table: table.into(),
        })
    }
    pub fn extension_legacy_review(&mut self, row: &LegacyExtensionRow) -> Result<ExtensionImport> {
        self.extension_check(&row.session)?;
        let result = self.transport.request(
            "POST",
            "/api/agent/extensions/import-review",
            Some(&row.session),
            Some(json!({"source_table":row.table,"row_number":row.row_number})),
        );
        self.extension_check(&row.session)?;
        let value = result?;
        if value["provenance"]["kind"] != "legacy"
            || value["provenance"]["source_table"] != row.table
            || value["provenance"]["row_number"] != row.row_number
            || value["provenance"]["source_sha256"] != value["source_sha256"]
        {
            return Err("Original extension review differs from its selected row.".into());
        }
        let kind = match row.table.as_str() {
            "skills" => ExtensionKind::Skill,
            "methods" => ExtensionKind::Method,
            "tool_recipes" => ExtensionKind::Recipe,
            _ => return Err("Invalid original extension kind.".into()),
        };
        let body = json!({"request_id":crate::task_bridge::new_task_request_id()?,"source_table":row.table,"row_number":row.row_number,"expected_source_sha256":value["source_sha256"]});
        self.extension_import_review(
            &row.session,
            "/api/agent/extensions/import",
            body,
            value,
            Some(kind),
        )
    }
    fn extension_import_review(
        &self,
        session: &str,
        path: &'static str,
        body: Value,
        value: Value,
        kind: Option<ExtensionKind>,
    ) -> Result<ExtensionImport> {
        if !digest(&value["source_sha256"])
            || value["enabled_on_import"] != false
            || value["execution"] != "none"
        {
            return Err("Import review did not preserve disabled, inert data.".into());
        }
        let definition: ExtensionDefinition = serde_json::from_value(value["definition"].clone())
            .map_err(|_| "Invalid imported definition.")?;
        definition.validate()?;
        if kind.is_some_and(|k| k != definition.kind()) {
            return Err("Imported definition kind changed.".into());
        }
        Ok(ExtensionImport {
            session: session.into(),
            path,
            body,
            definition,
            provenance: value["provenance"].clone(),
            submitted: Arc::new(AtomicBool::new(false)),
        })
    }
    /// `retry` is an explicit user action after uncertainty, never an automatic retry.
    pub fn extension_import(
        &mut self,
        reviewed: &ExtensionImport,
        retry: bool,
    ) -> Result<ExtensionReview> {
        self.extension_check(&reviewed.session)?;
        if retry != reviewed.submitted() {
            return Err("Use the retained import action; do not create a second request.".into());
        }
        reviewed.submitted.store(true, Ordering::SeqCst);
        let result = self.transport.request(
            "POST",
            reviewed.path,
            Some(&reviewed.session),
            Some(reviewed.body.clone()),
        );
        self.extension_check(&reviewed.session)?;
        let value = result?;
        let record: ExtensionRecord =
            serde_json::from_value(value["extension"].clone()).map_err(|_| {
                "Import outcome needs inspection or an explicit retry of the retained request."
            })?;
        valid_record(&record)?;
        if record.definition != reviewed.definition
            || record.summary.provenance != reviewed.provenance
            || record.summary.enabled
            || record.summary.deleted
            || record.summary.revision != 1
            || value["execution"] != "none"
            || value["replayed"].as_bool().is_none()
        {
            return Err("Import receipt differs from its exact reviewed definition.".into());
        }
        Ok(ExtensionReview {
            session: reviewed.session.clone(),
            record,
            used: Arc::new(AtomicBool::new(false)),
        })
    }
    pub fn extension_purge(&mut self, review: &ExtensionReview, confirm_id: &str) -> Result<()> {
        self.extension_check(&review.session)?;
        if !review.record.summary.deleted || confirm_id != review.record.summary.id {
            return Err("Confirm permanent removal of this exact trashed extension.".into());
        }
        consume(&review.used)?;
        let result=self.transport.request("POST",&format!("/api/agent/extensions/{}/purge",encode_component(confirm_id)),Some(&review.session),Some(json!({"expected_revision":review.record.summary.revision,"confirm_id":confirm_id,"permanent":true})));
        self.extension_check(&review.session)?;
        let value = result?;
        if value["id"] != confirm_id
            || value["purged"] != true
            || value["task_history_preserved"] != true
            || value["import_receipts_preserved"] != true
        {
            return Err("Permanent-removal outcome needs a fresh list.".into());
        }
        Ok(())
    }
}
