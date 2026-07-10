use super::{ProposedPlanRecord, Store};
use anyhow::Result;
use sqlx::Row;

fn from_row(row: sqlx::sqlite::SqliteRow) -> Result<ProposedPlanRecord> {
    Ok(ProposedPlanRecord {
        id: row.try_get("id")?,
        frame_id: row.try_get("frame_id")?,
        codex_thread_id: row.try_get("codex_thread_id")?,
        codex_turn_id: row.try_get("codex_turn_id")?,
        revision: row.try_get("revision")?,
        markdown: row.try_get("markdown")?,
        status: row.try_get("status")?,
        mode: row.try_get("mode")?,
        progress_json: row.try_get("progress_json")?,
        runtime_config_json: row.try_get("runtime_config_json")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

impl Store {
    pub async fn next_proposed_plan_revision(&self, frame_id: &str) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(revision), 0) + 1 FROM proposed_plans WHERE frame_id=?",
        )
        .bind(frame_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    pub async fn save_proposed_plan(&self, plan: &ProposedPlanRecord) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "UPDATE proposed_plans SET status='superseded',updated_at=? \
             WHERE frame_id=? AND revision<? AND status IN ('pending','draft')",
        )
        .bind(plan.updated_at)
        .bind(&plan.frame_id)
        .bind(plan.revision)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO proposed_plans(\
             id,frame_id,codex_thread_id,codex_turn_id,revision,markdown,status,mode,\
             progress_json,runtime_config_json,created_at,updated_at) \
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?) \
             ON CONFLICT(id) DO UPDATE SET \
             codex_thread_id=excluded.codex_thread_id, \
             codex_turn_id=excluded.codex_turn_id, markdown=excluded.markdown, \
             status=excluded.status, mode=excluded.mode, \
             progress_json=excluded.progress_json, \
             runtime_config_json=excluded.runtime_config_json, \
             updated_at=excluded.updated_at",
        )
        .bind(&plan.id)
        .bind(&plan.frame_id)
        .bind(plan.codex_thread_id.as_deref())
        .bind(plan.codex_turn_id.as_deref())
        .bind(plan.revision)
        .bind(&plan.markdown)
        .bind(&plan.status)
        .bind(&plan.mode)
        .bind(&plan.progress_json)
        .bind(&plan.runtime_config_json)
        .bind(plan.created_at)
        .bind(plan.updated_at)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn latest_proposed_plan(&self, frame_id: &str) -> Result<Option<ProposedPlanRecord>> {
        let row = sqlx::query(
            "SELECT id,frame_id,codex_thread_id,codex_turn_id,revision,markdown,status,mode,\
             progress_json,runtime_config_json,created_at,updated_at \
             FROM proposed_plans WHERE frame_id=? ORDER BY revision DESC LIMIT 1",
        )
        .bind(frame_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(from_row).transpose()
    }

    pub async fn list_proposed_plans(&self, frame_id: &str) -> Result<Vec<ProposedPlanRecord>> {
        let rows = sqlx::query(
            "SELECT id,frame_id,codex_thread_id,codex_turn_id,revision,markdown,status,mode,\
             progress_json,runtime_config_json,created_at,updated_at \
             FROM proposed_plans WHERE frame_id=? ORDER BY revision",
        )
        .bind(frame_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(from_row).collect()
    }

    pub async fn update_proposed_plan_state(
        &self,
        id: &str,
        status: &str,
        progress_json: Option<&str>,
    ) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let result = sqlx::query(
            "UPDATE proposed_plans SET status=?, \
             progress_json=COALESCE(?,progress_json), updated_at=? WHERE id=?",
        )
        .bind(status)
        .bind(progress_json)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Atomically claim a still-actionable proposal. This prevents a double
    /// click or two windows from executing the same approved plan twice.
    pub async fn claim_proposed_plan(
        &self,
        id: &str,
        revision: i64,
        next_status: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE proposed_plans SET status=?,updated_at=? \
             WHERE id=? AND revision=? AND status IN ('pending','draft')",
        )
        .bind(next_status)
        .bind(chrono::Utc::now().timestamp())
        .bind(id)
        .bind(revision)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }
}
