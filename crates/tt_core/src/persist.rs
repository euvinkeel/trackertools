//! Project files (DESIGN §12): one SQLite database per project (`.ttproj`).
//!
//! - **Components**: every component type declared [`Class::Document`] and
//!   reflected is saved generically (RON via bevy_reflect), so new features
//!   persist with no extra code. Entity references inside components (e.g.
//!   operator inputs) are remapped on load by walking the reflected value.
//! - **Signals**: chunks are content-addressed (BLAKE3) and LZ4-compressed,
//!   so a save after a small edit writes only the chunks that changed.
//! - **Deleted entities** (disabled, kept only for undo) are not saved, nor
//!   are signals no saved component refers to; undo history does not outlive
//!   the session.
//! - **Meta**: format and version for migrations, plus app-level entries
//!   (the media path, …) through [`ProjectMeta`].
//!
//! Saving runs in one SQLite transaction: a crash never leaves half a project.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::prelude::*;
use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent};
use bevy_ecs::resource::IsResource;
use bevy_reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
use bevy_reflect::{FromReflect, PartialReflect, ReflectMut, ReflectRef, TypeRegistry};
use rusqlite::{Connection, OptionalExtension, params};
use serde::de::DeserializeSeed;

use crate::meta::{Class, ComponentMetas};
use crate::signal::{Signal, SignalId, SignalStore};

pub const FORMAT: &str = "trackertools.project";
pub const VERSION: i64 = 1;

/// App-level entries saved with the project (e.g. `media.path`).
#[derive(Resource, Default, Debug, Clone, PartialEq)]
pub struct ProjectMeta(pub BTreeMap<String, String>);

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct SaveStats {
    pub entities: usize,
    pub components: usize,
    pub signals: usize,
    pub chunks: usize,
    /// Chunks whose content wasn't in the file yet (bytes actually written).
    pub new_chunks: usize,
    pub new_bytes: usize,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS components (entity INTEGER NOT NULL, type TEXT NOT NULL, data TEXT NOT NULL,
                                       PRIMARY KEY (entity, type));
CREATE TABLE IF NOT EXISTS signals (id INTEGER PRIMARY KEY, channels INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS signal_chunks (signal INTEGER NOT NULL, idx INTEGER NOT NULL, hash BLOB NOT NULL,
                                          PRIMARY KEY (signal, idx));
CREATE TABLE IF NOT EXISTS blobs (hash BLOB PRIMARY KEY, data BLOB NOT NULL);
";

/// Component types that make up the document: declared `Document` and reflected.
fn document_types(world: &World) -> Vec<(String, ReflectComponent)> {
    let metas = world.resource::<ComponentMetas>();
    let registry = world.resource::<AppTypeRegistry>().read();
    let mut out: Vec<(String, ReflectComponent)> = registry
        .iter()
        .filter(|r| metas.get(r.type_id()).is_some_and(|m| m.class == Class::Document))
        .filter_map(|r| Some((r.type_info().type_path().to_string(), r.data::<ReflectComponent>()?.clone())))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn save(world: &mut World, path: &Path) -> Result<SaveStats> {
    let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch(SCHEMA)?;
    let types = document_types(world);
    let mut stats = SaveStats::default();

    // Serialize components first (no DB borrow while holding world references).
    let mut rows: Vec<(u64, String, String)> = Vec::new();
    // The signals those components refer to: the only ones saved (a deleted
    // entity's signals stay in the store for undo, but not in the file).
    let mut used: HashSet<SignalId> = HashSet::new();
    {
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let mut q = world.query_filtered::<EntityRef, (Without<Disabled>, Without<IsResource>)>();
        for entity in q.iter(world) {
            let mut any = false;
            for (type_path, rc) in &types {
                let Some(value) = rc.reflect(entity) else { continue };
                let ser = TypedReflectSerializer::new(value.as_partial_reflect(), &registry);
                let text = ron::to_string(&ser).with_context(|| format!("serializing {type_path}"))?;
                rows.push((entity.id().to_bits(), type_path.clone(), text));
                collect_signals(value.as_partial_reflect(), &mut used);
                any = true;
            }
            stats.entities += usize::from(any);
        }
    }
    stats.components = rows.len();

    let tx = conn.transaction()?;
    tx.execute("DELETE FROM meta", [])?;
    tx.execute("INSERT INTO meta (key, value) VALUES ('format', ?1), ('version', ?2)", params![FORMAT, VERSION.to_string()])?;
    if let Some(meta) = world.get_resource::<ProjectMeta>() {
        for (k, v) in &meta.0 {
            tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", params![format!("app.{k}"), v])?;
        }
    }
    tx.execute("DELETE FROM components", [])?;
    {
        let mut ins = tx.prepare("INSERT INTO components (entity, type, data) VALUES (?1, ?2, ?3)")?;
        for (e, t, d) in &rows {
            ins.execute(params![*e as i64, t, d])?;
        }
    }

    tx.execute("DELETE FROM signals", [])?;
    tx.execute("DELETE FROM signal_chunks", [])?;
    {
        let store = world.resource::<SignalStore>();
        let mut ids: Vec<SignalId> = store.ids().filter(|id| used.contains(id)).collect();
        ids.sort();
        let mut ins_sig = tx.prepare("INSERT INTO signals (id, channels) VALUES (?1, ?2)")?;
        let mut ins_chunk = tx.prepare("INSERT INTO signal_chunks (signal, idx, hash) VALUES (?1, ?2, ?3)")?;
        let mut has_blob = tx.prepare("SELECT 1 FROM blobs WHERE hash = ?1")?;
        let mut ins_blob = tx.prepare("INSERT INTO blobs (hash, data) VALUES (?1, ?2)")?;
        for id in ids {
            let sig = store.get(id).unwrap();
            ins_sig.execute(params![id.0 as i64, sig.channels() as i64])?;
            stats.signals += 1;
            for (idx, bytes) in sig.chunk_bytes() {
                let hash = blake3::hash(&bytes);
                let hash = hash.as_bytes().as_slice();
                if has_blob.query_row(params![hash], |_| Ok(())).optional()?.is_none() {
                    let packed = lz4_flex::compress_prepend_size(&bytes);
                    stats.new_bytes += packed.len();
                    stats.new_chunks += 1;
                    ins_blob.execute(params![hash, packed])?;
                }
                ins_chunk.execute(params![id.0 as i64, idx, hash])?;
                stats.chunks += 1;
            }
        }
    }
    // Chunks nothing refers to anymore.
    tx.execute("DELETE FROM blobs WHERE hash NOT IN (SELECT hash FROM signal_chunks)", [])?;
    tx.commit()?;
    Ok(stats)
}

/// Load a project into `world` (built with the same modules; normally fresh).
pub fn load(world: &mut World, path: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", path.display()))?;
    let meta: HashMap<String, String> = conn
        .prepare("SELECT key, value FROM meta")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if meta.get("format").map(String::as_str) != Some(FORMAT) {
        bail!("{} is not a trackertools project", path.display());
    }
    let version: i64 = meta.get("version").and_then(|v| v.parse().ok()).unwrap_or(0);
    if version > VERSION {
        bail!("{} was saved by a newer version (format {version}, this build reads {VERSION})", path.display());
    }
    // (Migrations from older versions go here as the format evolves.)

    // Signals.
    let channels: Vec<(i64, i64)> =
        conn.prepare("SELECT id, channels FROM signals")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut chunk_q = conn.prepare(
        "SELECT signal_chunks.idx, blobs.data FROM signal_chunks JOIN blobs ON blobs.hash = signal_chunks.hash
         WHERE signal_chunks.signal = ?1",
    )?;
    let loaded: Vec<SignalId> = channels.iter().map(|(id, _)| SignalId(*id as u64)).collect();
    for (id, ch) in channels {
        let packed: Vec<(i64, Vec<u8>)> = chunk_q.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let raw: Vec<(i64, Vec<u8>)> = packed
            .into_iter()
            .map(|(k, p)| Ok((k, lz4_flex::decompress_size_prepended(&p).context("corrupt signal chunk")?)))
            .collect::<Result<_>>()?;
        let signal = Signal::from_chunk_bytes(ch as usize, raw.iter().map(|(k, b)| (*k, b.as_slice())))?;
        world.resource_mut::<SignalStore>().insert(SignalId(id as u64), signal);
    }

    // Entities: one fresh entity per saved id, then components with references remapped.
    let rows: Vec<(i64, String, String)> = conn
        .prepare("SELECT entity, type, data FROM components ORDER BY entity, type")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    // Fresh entities in the saved ones' creation order (their index), so
    // anything listed by creation (the outliner, the timeline) keeps its order.
    let mut olds: Vec<u64> = rows.iter().map(|(old, _, _)| *old as u64).collect();
    olds.sort_by_key(|o| Entity::try_from_bits(*o).map_or(u32::MAX, |e| e.index_u32()));
    olds.dedup();
    let mut map: HashMap<u64, Entity> = HashMap::new();
    for old in olds {
        map.entry(old).or_insert_with(|| world.spawn_empty().id());
    }
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    for (old, type_path, data) in rows {
        let Some(registration) = registry.get_with_type_path(&type_path) else {
            tracing::warn!("project has component {type_path}, unknown to this build; skipped");
            continue;
        };
        let Some(rc) = registration.data::<ReflectComponent>() else { continue };
        let mut de = ron::Deserializer::from_str(&data).with_context(|| format!("parsing {type_path}"))?;
        let mut value: Box<dyn PartialReflect> =
            TypedReflectDeserializer::new(registration, &registry).deserialize(&mut de).with_context(|| format!("reading {type_path}"))?;
        map_entities(value.as_mut(), &map);
        let target = map[&(old as u64)];
        rc.insert(&mut world.entity_mut(target), value.as_ref(), &registry);
    }
    drop(registry);

    // Signals nothing loaded refers to (older saves kept deleted entities' signals) are dropped.
    let types = document_types(world);
    let mut used: HashSet<SignalId> = HashSet::new();
    for e in map.values() {
        let entity = world.entity(*e);
        for (_, rc) in &types {
            if let Some(value) = rc.reflect(entity) {
                collect_signals(value.as_partial_reflect(), &mut used);
            }
        }
    }
    let unused: Vec<SignalId> = loaded.into_iter().filter(|id| !used.contains(id)).collect();
    if !unused.is_empty() {
        tracing::info!("{}: dropped {} signals nothing refers to", path.display(), unused.len());
        let mut store = world.resource_mut::<SignalStore>();
        for id in unused {
            store.remove(id);
        }
    }

    let app_meta = meta.into_iter().filter_map(|(k, v)| Some((k.strip_prefix("app.")?.to_string(), v))).collect();
    world.insert_resource(ProjectMeta(app_meta));
    Ok(())
}

/// Remove the current document: every entity carrying a document component
/// (including deleted ones kept for undo), all signals, and the history.
/// Session state (layout, transport, selection) is kept; selection is cleared.
pub fn clear_document(world: &mut World) {
    let types = document_types(world);
    let mut q = world.query_filtered::<EntityRef, (Without<IsResource>, Or<(With<Disabled>, Without<Disabled>)>)>();
    let doomed: Vec<Entity> = q.iter(world).filter(|e| types.iter().any(|(_, rc)| rc.contains(*e))).map(|e| e.id()).collect();
    for e in doomed {
        world.despawn(e);
    }
    *world.resource_mut::<SignalStore>() = SignalStore::default();
    world.resource_mut::<crate::history::History>().clear();
    world.resource_mut::<crate::selection::Selection>().clear();
    world.resource_mut::<ProjectMeta>().0.clear();
    world.resource_mut::<crate::op::OpGraph>().mark_stale();
}

/// Replace every `Entity` inside a reflected value using `map` (saved id → new entity).
fn map_entities(value: &mut dyn PartialReflect, map: &HashMap<u64, Entity>) {
    if let Some(e) = value.try_downcast_mut::<Entity>() {
        if let Some(new) = map.get(&e.to_bits()) {
            *e = *new;
        } else {
            *e = Entity::PLACEHOLDER; // dangling reference: treated as disconnected
        }
        return;
    }
    match value.reflect_mut() {
        ReflectMut::Struct(s) => {
            for i in 0..s.field_len() {
                if let Some(f) = s.field_at_mut(i) {
                    map_entities(f, map);
                }
            }
        }
        ReflectMut::TupleStruct(s) => {
            for i in 0..s.field_len() {
                if let Some(f) = s.field_mut(i) {
                    map_entities(f, map);
                }
            }
        }
        ReflectMut::Tuple(t) => {
            for i in 0..t.field_len() {
                if let Some(f) = t.field_mut(i) {
                    map_entities(f, map);
                }
            }
        }
        ReflectMut::List(l) => {
            for i in 0..l.len() {
                if let Some(f) = l.get_mut(i) {
                    map_entities(f, map);
                }
            }
        }
        ReflectMut::Array(a) => {
            for i in 0..a.len() {
                if let Some(f) = a.get_mut(i) {
                    map_entities(f, map);
                }
            }
        }
        ReflectMut::Enum(e) => {
            for i in 0..e.field_len() {
                if let Some(f) = e.field_at_mut(i) {
                    map_entities(f, map);
                }
            }
        }
        // Maps/sets of entities aren't used by any component yet.
        _ => {}
    }
}

/// Every `SignalId` inside a reflected value (`Output`, `Through`, …), found
/// the way [`map_entities`] finds entities, so new components are covered.
fn collect_signals(value: &dyn PartialReflect, out: &mut HashSet<SignalId>) {
    if value.represents::<SignalId>() {
        out.extend(SignalId::from_reflect(value));
        return;
    }
    match value.reflect_ref() {
        ReflectRef::Struct(s) => (0..s.field_len()).filter_map(|i| s.field_at(i)).for_each(|f| collect_signals(f, out)),
        ReflectRef::TupleStruct(s) => (0..s.field_len()).filter_map(|i| s.field(i)).for_each(|f| collect_signals(f, out)),
        ReflectRef::Tuple(t) => (0..t.field_len()).filter_map(|i| t.field(i)).for_each(|f| collect_signals(f, out)),
        ReflectRef::List(l) => l.iter().for_each(|f| collect_signals(f, out)),
        ReflectRef::Array(a) => a.iter().for_each(|f| collect_signals(f, out)),
        ReflectRef::Map(m) => m.iter().for_each(|(k, v)| {
            collect_signals(k, out);
            collect_signals(v, out);
        }),
        ReflectRef::Set(s) => s.iter().for_each(|f| collect_signals(f, out)),
        ReflectRef::Enum(e) => (0..e.field_len()).filter_map(|i| e.field_at(i)).for_each(|f| collect_signals(f, out)),
        _ => {}
    }
}

/// Used by tests and tools: a registry read guard for ad-hoc reflection.
pub fn with_registry<R>(world: &World, f: impl FnOnce(&TypeRegistry) -> R) -> R {
    f(&world.resource::<AppTypeRegistry>().read())
}

pub struct PersistModule;

impl crate::app::Module for PersistModule {
    fn build(&self, app: &mut crate::app::AppBuilder) {
        app.declare::<ProjectMeta>(Class::Document).init_resource::<ProjectMeta>();
    }
}
