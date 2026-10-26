use std::path::{Path, PathBuf};

use sea_query::{Expr, ExprTrait, OnConflict, Order, Query};

use crate::contracts::guide::Guide;
use crate::core::error::Result;
use crate::platform::database::{Database, Guides};
use crate::platform::persistence::SettingsStore;

/// Guides kept per subject, newest first; older heads fall off.
const KEEP_PER_SUBJECT: usize = 8;

/// Guides keyed by (subject, base, head, model), so reopening the same head
/// shows the guide again without another model call.
#[derive(Debug, Clone)]
pub struct GuideStore {
    dir: PathBuf,
}

impl GuideStore {
    pub fn for_settings_store(settings_store: &SettingsStore) -> Self {
        Self {
            dir: settings_store.dir().to_path_buf(),
        }
    }

    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The guide for exactly this base and head if one is cached, else the
    /// subject's newest guide, which the caller shows as out of date.
    pub fn load(&self, subject: &str, base: &str, head: &str) -> Result<Option<Guide>> {
        let select = Query::select()
            .column(Guides::Json)
            .from(Guides::Table)
            .and_where(Expr::col(Guides::Subject).eq(subject))
            .order_by(Guides::UpdatedMs, Order::Desc)
            .to_owned();
        let guides: Vec<Guide> = self
            .database()?
            .query(&select, |row| row.get::<_, String>(0))?
            .iter()
            .filter_map(|json| {
                serde_json::from_str(json)
                    .map_err(|error| tracing::warn!("skipping unreadable guide: {error}"))
                    .ok()
            })
            .collect();
        let exact = guides.iter().position(|guide| guide.is_for(base, head));
        Ok(match exact {
            Some(at) => guides.into_iter().nth(at),
            None => guides.into_iter().next(),
        })
    }

    pub fn save(&self, subject: &str, guide: &Guide) -> Result<()> {
        let key = format!("{subject}\n{}\n{}\n{}", guide.base, guide.head, guide.model);
        let db = self.database()?;
        db.execute(
            &Query::insert()
                .into_table(Guides::Table)
                .columns([
                    Guides::Key,
                    Guides::Subject,
                    Guides::UpdatedMs,
                    Guides::Json,
                ])
                .values_panic([
                    key.into(),
                    subject.into(),
                    i64::try_from(guide.created_ms).unwrap_or(i64::MAX).into(),
                    serde_json::to_string(guide)?.into(),
                ])
                .on_conflict(
                    OnConflict::column(Guides::Key)
                        .update_columns([Guides::UpdatedMs, Guides::Json])
                        .to_owned(),
                )
                .to_owned(),
        )?;
        let stale = Query::select()
            .column(Guides::Key)
            .from(Guides::Table)
            .and_where(Expr::col(Guides::Subject).eq(subject))
            .order_by(Guides::UpdatedMs, Order::Desc)
            .offset(KEEP_PER_SUBJECT as u64)
            .limit(u64::MAX >> 1)
            .to_owned();
        let stale = db.query(&stale, |row| row.get::<_, String>(0))?;
        if !stale.is_empty() {
            db.execute(
                &Query::delete()
                    .from_table(Guides::Table)
                    .and_where(Expr::col(Guides::Key).is_in(stale))
                    .to_owned(),
            )?;
        }
        Ok(())
    }

    fn database(&self) -> Result<Database> {
        Database::in_dir(&self.dir)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::GuideStore;
    use crate::contracts::guide::Guide;

    fn guide(head: &str, model: &str, created_ms: u64) -> Guide {
        Guide {
            overview: format!("Guide for {head}"),
            steps: Vec::new(),
            model: model.to_owned(),
            base: "base".to_owned(),
            head: head.to_owned(),
            created_ms,
        }
    }

    #[test]
    fn exact_heads_win_and_other_heads_come_back_as_the_newest() {
        let dir = TempDir::new().unwrap();
        let store = GuideStore::in_dir(dir.path());
        store
            .save("pr:github/a/b#1", &guide("h1", "sonnet", 10))
            .unwrap();
        store
            .save("pr:github/a/b#1", &guide("h2", "sonnet", 20))
            .unwrap();
        store
            .save("pr:github/a/b#2", &guide("h9", "sonnet", 30))
            .unwrap();

        let exact = store.load("pr:github/a/b#1", "base", "h1").unwrap();
        assert_eq!(exact.map(|g| g.head), Some("h1".to_owned()));
        let newest = store.load("pr:github/a/b#1", "base", "h3").unwrap();
        assert_eq!(newest.map(|g| g.head), Some("h2".to_owned()));
        assert!(
            store
                .load("pr:github/a/b#3", "base", "h1")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn regenerating_replaces_the_row_and_old_heads_are_pruned() {
        let dir = TempDir::new().unwrap();
        let store = GuideStore::in_dir(dir.path());
        let mut first = guide("h0", "sonnet", 1);
        store.save("s", &first).unwrap();
        first.overview = "Regenerated".to_owned();
        first.created_ms = 2;
        store.save("s", &first).unwrap();
        assert_eq!(
            store.load("s", "base", "h0").unwrap().map(|g| g.overview),
            Some("Regenerated".to_owned())
        );

        for at in 1..=super::KEEP_PER_SUBJECT as u64 {
            store
                .save("s", &guide(&format!("h{at}"), "sonnet", 10 + at))
                .unwrap();
        }
        assert!(
            store
                .load("s", "base", "h0")
                .unwrap()
                .is_some_and(|g| g.head != "h0"),
            "the oldest head fell off"
        );
    }
}
