use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use crate::{
    ListSessions, ProgressiveBatchJobRow, ProgressiveBatchSegmentRow, SessionActionItemRow,
    SessionDocumentRow, SessionListItem, SessionParticipantRow, SessionRow, SessionTranscriptRow,
};

pub const MAX_SESSION_LIST_LIMIT: u32 = 500;

const SESSION_LIST_COLUMNS: &str = "
    SELECT id, title, kind, status, created_at, updated_at, started_at, ended_at, series_id
    FROM sessions
";

const SESSION_COLUMNS: &str = "
    SELECT id, workspace_id, owner_user_id, title, kind, status, created_at, updated_at,
           started_at, ended_at, timezone, language, event_id, external_event_id,
           external_provider, series_id, source_apps_json, event_json, folder_path, slug,
           metadata_json
    FROM sessions
";

const SESSION_DOCUMENT_COLUMNS: &str = "
    SELECT document.id, document.workspace_id, document.session_id, document.kind,
           document.template_id, document.title, document.body_format, document.body,
           document.source_hash, document.generation_metadata_json, document.sort_order,
           document.created_by, document.updated_by, document.created_at, document.updated_at
    FROM session_documents AS document
    JOIN sessions AS session ON session.id = document.session_id AND session.deleted_at IS NULL
";

const SESSION_TRANSCRIPT_COLUMNS: &str = "
    SELECT transcript.id, transcript.workspace_id, transcript.owner_user_id,
           transcript.session_id, transcript.source, transcript.provider, transcript.model,
           transcript.language, transcript.started_at_ms, transcript.ended_at_ms,
           transcript.audio_attachment_id, transcript.memo, transcript.words_json,
           transcript.speaker_hints_json, transcript.metadata_json, transcript.created_at,
           transcript.updated_at
    FROM transcripts AS transcript
    JOIN sessions AS session ON session.id = transcript.session_id AND session.deleted_at IS NULL
";

const SESSION_PARTICIPANT_COLUMNS: &str = "
    SELECT participant.id, participant.workspace_id, participant.owner_user_id,
           participant.session_id, participant.human_id,
           COALESCE(NULLIF(human.name, ''), participant.display_name) AS display_name,
           COALESCE(NULLIF(human.email, ''), participant.email) AS email,
           participant.role, participant.source, COALESCE(human.job_title, '') AS job_title,
           COALESCE(human.organization_id, '') AS organization_id,
           COALESCE(organization.name, '') AS organization_name, participant.metadata_json,
           participant.created_at, participant.updated_at
    FROM session_participants AS participant
    JOIN sessions AS session ON session.id = participant.session_id AND session.deleted_at IS NULL
    LEFT JOIN humans AS human ON human.id = participant.human_id AND human.deleted_at IS NULL
    LEFT JOIN organizations AS organization
      ON organization.id = human.organization_id AND organization.deleted_at IS NULL
";

const SESSION_ACTION_ITEM_COLUMNS: &str = "
    SELECT action_item.id, action_item.workspace_id, action_item.session_id,
           action_item.source_type, action_item.source_id, action_item.source_order,
           action_item.assignee_human_id, action_item.status, action_item.text,
           action_item.body_json, action_item.due_at, action_item.completed_at,
           action_item.created_by, action_item.updated_by, action_item.metadata_json,
           action_item.created_at, action_item.updated_at
    FROM action_items AS action_item
    JOIN sessions AS session ON session.id = action_item.session_id AND session.deleted_at IS NULL
";

pub async fn list_sessions(
    pool: &SqlitePool,
    input: ListSessions<'_>,
) -> Result<Vec<SessionListItem>, sqlx::Error> {
    let mut query = QueryBuilder::<Sqlite>::new(SESSION_LIST_COLUMNS);
    query.push(" WHERE deleted_at IS NULL");

    if let Some(search) = input.query.map(str::trim).filter(|query| !query.is_empty()) {
        query.push(" AND (instr(lower(title), lower(");
        query.push_bind(search);
        query.push(")) > 0 OR instr(lower(id), lower(");
        query.push_bind(search);
        query.push(")) > 0)");
    }

    if let Some(series_id) = input.series_id {
        query.push(" AND series_id = ");
        query.push_bind(series_id);
    }

    query.push(
        " ORDER BY COALESCE(NULLIF(started_at, ''), created_at) DESC, created_at DESC, id DESC",
    );
    query.push(" LIMIT ");
    query.push_bind(i64::from(input.limit.min(MAX_SESSION_LIST_LIMIT)));
    query.push(" OFFSET ");
    query.push_bind(i64::from(input.offset));

    query
        .build_query_as::<SessionListItem>()
        .fetch_all(pool)
        .await
}

pub async fn get_session(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Option<SessionRow>, sqlx::Error> {
    let mut query = QueryBuilder::<Sqlite>::new(SESSION_COLUMNS);
    query.push(" WHERE id = ");
    query.push_bind(session_id);
    query.push(" AND deleted_at IS NULL LIMIT 1");
    query
        .build_query_as::<SessionRow>()
        .fetch_optional(pool)
        .await
}

pub async fn list_session_documents(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Vec<SessionDocumentRow>, sqlx::Error> {
    let mut query = QueryBuilder::<Sqlite>::new(SESSION_DOCUMENT_COLUMNS);
    query.push(" WHERE document.session_id = ");
    query.push_bind(session_id);
    query.push(
        " AND document.deleted_at IS NULL
          ORDER BY document.sort_order, document.created_at, document.id",
    );
    query
        .build_query_as::<SessionDocumentRow>()
        .fetch_all(pool)
        .await
}

pub async fn get_session_note(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Option<SessionDocumentRow>, sqlx::Error> {
    let mut query = QueryBuilder::<Sqlite>::new(SESSION_DOCUMENT_COLUMNS);
    query.push(" WHERE document.session_id = ");
    query.push_bind(session_id);
    query.push(
        " AND document.kind = 'note' AND document.deleted_at IS NULL
          ORDER BY CASE WHEN document.id = ",
    );
    query.push_bind(session_id);
    query.push(" THEN 0 ELSE 1 END, document.created_at, document.id LIMIT 1");
    query
        .build_query_as::<SessionDocumentRow>()
        .fetch_optional(pool)
        .await
}

pub async fn list_session_transcripts(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Vec<SessionTranscriptRow>, sqlx::Error> {
    let mut query = QueryBuilder::<Sqlite>::new(SESSION_TRANSCRIPT_COLUMNS);
    query.push(" WHERE transcript.session_id = ");
    query.push_bind(session_id);
    query.push(
        " AND transcript.deleted_at IS NULL
          ORDER BY transcript.started_at_ms, transcript.created_at, transcript.id",
    );
    query
        .build_query_as::<SessionTranscriptRow>()
        .fetch_all(pool)
        .await
}

pub async fn list_session_participants(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Vec<SessionParticipantRow>, sqlx::Error> {
    let mut query = QueryBuilder::<Sqlite>::new(SESSION_PARTICIPANT_COLUMNS);
    query.push(" WHERE participant.session_id = ");
    query.push_bind(session_id);
    query.push(
        " AND participant.deleted_at IS NULL AND participant.source <> 'excluded'
          ORDER BY participant.created_at, participant.id",
    );
    query
        .build_query_as::<SessionParticipantRow>()
        .fetch_all(pool)
        .await
}

pub async fn list_session_action_items(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Vec<SessionActionItemRow>, sqlx::Error> {
    let mut query = QueryBuilder::<Sqlite>::new(SESSION_ACTION_ITEM_COLUMNS);
    query.push(" WHERE action_item.session_id = ");
    query.push_bind(session_id);
    query.push(
        " AND action_item.deleted_at IS NULL
          ORDER BY action_item.source_order, action_item.created_at, action_item.id",
    );
    query
        .build_query_as::<SessionActionItemRow>()
        .fetch_all(pool)
        .await
}

pub async fn list_recurring_sessions(
    pool: &SqlitePool,
    session_id: &str,
    limit: u32,
) -> Result<Vec<SessionListItem>, sqlx::Error> {
    let series_id: Option<String> = sqlx::query_scalar(
        "SELECT NULLIF(trim(series_id), '')
         FROM sessions
         WHERE id = ? AND deleted_at IS NULL
         LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    let Some(series_id) = series_id else {
        return Ok(Vec::new());
    };

    list_sessions(
        pool,
        ListSessions {
            query: None,
            series_id: Some(&series_id),
            limit,
            offset: 0,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use hypr_db_core::Db;

    use super::*;

    async fn test_db() -> Db {
        let db = Db::connect_memory_plain().await.unwrap();
        crate::prepare_schema(&db).await.unwrap();
        db
    }

    async fn insert_session(
        pool: &SqlitePool,
        id: &str,
        title: &str,
        started_at: &str,
        series_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO sessions
             (id, title, started_at, series_id, source_apps_json, event_json, metadata_json)
             VALUES (?, ?, ?, ?, '[{\"app\":\"zoom\"}]', '{\"event\":1}', '{\"source\":\"test\"}')",
        )
        .bind(id)
        .bind(title)
        .bind(started_at)
        .bind(series_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn list_sessions_filters_searches_and_paginates_deterministically() {
        let db = test_db().await;
        insert_session(
            db.pool(),
            "alpha-old",
            "Alpha Planning",
            "2026-01-01",
            "series-a",
        )
        .await;
        insert_session(
            db.pool(),
            "alpha-new",
            "ALPHA Review",
            "2026-02-01",
            "series-a",
        )
        .await;
        insert_session(db.pool(), "beta", "Beta Review", "2026-03-01", "series-b").await;
        sqlx::query("UPDATE sessions SET deleted_at = '2026-04-01' WHERE id = 'beta'")
            .execute(db.pool())
            .await
            .unwrap();

        let first = list_sessions(
            db.pool(),
            ListSessions {
                query: Some(" alpha "),
                series_id: Some("series-a"),
                limit: 1,
                offset: 0,
            },
        )
        .await
        .unwrap();
        let second = list_sessions(
            db.pool(),
            ListSessions {
                query: Some("alpha"),
                series_id: Some("series-a"),
                limit: 1,
                offset: 1,
            },
        )
        .await
        .unwrap();
        let by_id = list_sessions(
            db.pool(),
            ListSessions {
                query: Some("old"),
                series_id: None,
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();

        assert_eq!(first[0].id, "alpha-new");
        assert_eq!(second[0].id, "alpha-old");
        assert_eq!(by_id[0].id, "alpha-old");
    }

    #[tokio::test]
    async fn session_read_helpers_return_active_rows_and_preserve_opaque_json() {
        let db = test_db().await;
        insert_session(db.pool(), "session-1", "Planning", "2026-01-01", "").await;
        sqlx::query(
            "INSERT INTO session_documents
             (id, session_id, kind, body, sort_order, created_at)
             VALUES
             ('fallback', 'session-1', 'note', 'fallback body', 0, '2026-01-01'),
             ('session-1', 'session-1', 'note', 'canonical body', 1, '2026-02-01'),
             ('summary', 'session-1', 'summary', 'summary body', 2, '2026-03-01'),
             ('deleted', 'session-1', 'note', 'deleted body', 3, '2026-04-01')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("UPDATE session_documents SET deleted_at = '2026-05-01' WHERE id = 'deleted'")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO transcripts
             (id, session_id, started_at_ms, words_json, speaker_hints_json)
             VALUES ('later', 'session-1', 200, '[{\"word\":\"later\"}]', '[]'),
                    ('earlier', 'session-1', 100, '[{\"word\":\"earlier\"}]', '[]')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO organizations (id, name) VALUES ('org-1', 'Acme')")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO humans (id, organization_id, name, email, job_title)
             VALUES ('human-1', 'org-1', 'Alice', 'alice@example.com', 'Engineer')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO session_participants
               (id, session_id, human_id, display_name, email, created_at)
             VALUES ('second', 'session-1', 'human-1', 'Alias', 'alias@example.com', '2026-02-01'),
                    ('first', 'session-1', '', 'First', '', '2026-01-01'),
                    ('excluded', 'session-1', '', 'Excluded', '', '2026-03-01')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("UPDATE session_participants SET source = 'excluded' WHERE id = 'excluded'")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO action_items (id, session_id, source_order, text, body_json)
             VALUES ('action-2', 'session-1', 2, 'Second', '{\"rank\":2}'),
                    ('action-1', 'session-1', 1, 'First', '{\"rank\":1}')",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let session = get_session(db.pool(), "session-1").await.unwrap().unwrap();
        let documents = list_session_documents(db.pool(), "session-1")
            .await
            .unwrap();
        let note = get_session_note(db.pool(), "session-1")
            .await
            .unwrap()
            .unwrap();
        let transcripts = list_session_transcripts(db.pool(), "session-1")
            .await
            .unwrap();
        let participants = list_session_participants(db.pool(), "session-1")
            .await
            .unwrap();
        let action_items = list_session_action_items(db.pool(), "session-1")
            .await
            .unwrap();

        assert_eq!(session.source_apps_json, "[{\"app\":\"zoom\"}]");
        assert_eq!(
            documents
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["fallback", "session-1", "summary"]
        );
        assert_eq!(note.id, "session-1");
        assert_eq!(
            transcripts
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["earlier", "later"]
        );
        assert_eq!(
            participants
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert_eq!(participants[1].display_name, "Alice");
        assert_eq!(participants[1].email, "alice@example.com");
        assert_eq!(participants[1].job_title, "Engineer");
        assert_eq!(participants[1].organization_id, "org-1");
        assert_eq!(participants[1].organization_name, "Acme");
        assert_eq!(
            action_items
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["action-1", "action-2"]
        );
        assert_eq!(action_items[0].body_json, "{\"rank\":1}");

        sqlx::query(
            "UPDATE session_documents SET deleted_at = '2026-06-01' WHERE id = 'session-1'",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let fallback = get_session_note(db.pool(), "session-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fallback.id, "fallback");

        sqlx::query("UPDATE sessions SET deleted_at = '2026-07-01' WHERE id = 'session-1'")
            .execute(db.pool())
            .await
            .unwrap();
        assert!(
            list_session_documents(db.pool(), "session-1")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            list_session_transcripts(db.pool(), "session-1")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            list_session_participants(db.pool(), "session-1")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            list_session_action_items(db.pool(), "session-1")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn recurring_sessions_resolve_series_and_return_newest_first() {
        let db = test_db().await;
        insert_session(db.pool(), "old", "Weekly", "2026-01-01", "series-a").await;
        insert_session(db.pool(), "current", "Weekly", "2026-02-01", "series-a").await;
        insert_session(db.pool(), "new", "Weekly", "2026-03-01", "series-a").await;
        insert_session(db.pool(), "standalone", "One-off", "2026-04-01", "").await;

        let recurring = list_recurring_sessions(db.pool(), "current", 2)
            .await
            .unwrap();
        let standalone = list_recurring_sessions(db.pool(), "standalone", 10)
            .await
            .unwrap();
        let missing = list_recurring_sessions(db.pool(), "missing", 10)
            .await
            .unwrap();

        assert_eq!(
            recurring
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["new", "current"]
        );
        assert!(standalone.is_empty());
        assert!(missing.is_empty());
    }
}

// ── Progressive Batch ──────────────────────────────────────────────

pub async fn create_progressive_batch_job(
    pool: &SqlitePool,
    job: &ProgressiveBatchJobRow,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO progressive_batch_jobs
         (id, session_id, status, provider, model, base_url, language,
          segment_duration_ms, overlap_ms, max_concurrency,
          total_segments, completed_segments, failed_segments, abandoned_segments,
          created_at, updated_at, completed_at, error)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&job.id)
    .bind(&job.session_id)
    .bind(&job.status)
    .bind(&job.provider)
    .bind(&job.model)
    .bind(&job.base_url)
    .bind(&job.language)
    .bind(job.segment_duration_ms)
    .bind(job.overlap_ms)
    .bind(job.max_concurrency)
    .bind(job.total_segments)
    .bind(job.completed_segments)
    .bind(job.failed_segments)
    .bind(job.abandoned_segments)
    .bind(&job.created_at)
    .bind(&job.updated_at)
    .bind(&job.completed_at)
    .bind(&job.error)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_progressive_batch_job_status(
    pool: &SqlitePool,
    id: &str,
    status: &str,
    completed_at: Option<&str>,
    error: Option<&str>,
    total_segments: i64,
    completed_segments: i64,
    failed_segments: i64,
    abandoned_segments: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE progressive_batch_jobs
         SET status = ?, completed_at = ?, error = ?,
             total_segments = ?, completed_segments = ?,
             failed_segments = ?, abandoned_segments = ?,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?",
    )
    .bind(status)
    .bind(completed_at)
    .bind(error)
    .bind(total_segments)
    .bind(completed_segments)
    .bind(failed_segments)
    .bind(abandoned_segments)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_progressive_batch_job(
    pool: &SqlitePool,
    id: &str,
) -> Result<Option<ProgressiveBatchJobRow>, sqlx::Error> {
    sqlx::query_as::<_, ProgressiveBatchJobRow>("SELECT * FROM progressive_batch_jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list_progressive_batch_jobs_by_session(
    pool: &SqlitePool,
    session_id: &str,
    status_filter: Option<&[&str]>,
) -> Result<Vec<ProgressiveBatchJobRow>, sqlx::Error> {
    let mut query =
        QueryBuilder::<Sqlite>::new("SELECT * FROM progressive_batch_jobs WHERE session_id = ");
    query.push_bind(session_id);
    if let Some(statuses) = status_filter {
        query.push(" AND status IN (");
        for (i, s) in statuses.iter().enumerate() {
            if i > 0 {
                query.push(", ");
            }
            query.push_bind(s);
        }
        query.push(")");
    }
    query.push(" ORDER BY created_at DESC");
    query
        .build_query_as::<ProgressiveBatchJobRow>()
        .fetch_all(pool)
        .await
}

pub async fn insert_progressive_batch_segment(
    pool: &SqlitePool,
    seg: &ProgressiveBatchSegmentRow,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO progressive_batch_segments
         (id, job_id, segment_index, global_start_ms, global_end_ms,
          status, retry_count, max_retries, error, response_json,
          created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&seg.id)
    .bind(&seg.job_id)
    .bind(seg.segment_index)
    .bind(seg.global_start_ms)
    .bind(seg.global_end_ms)
    .bind(&seg.status)
    .bind(seg.retry_count)
    .bind(seg.max_retries)
    .bind(&seg.error)
    .bind(&seg.response_json)
    .bind(&seg.created_at)
    .bind(&seg.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_progressive_batch_segment_status(
    pool: &SqlitePool,
    id: &str,
    status: &str,
    error: Option<&str>,
    response_json: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE progressive_batch_segments
         SET status = ?, error = ?, response_json = ?,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?",
    )
    .bind(status)
    .bind(error)
    .bind(response_json)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_completed_progressive_batch_segments(
    pool: &SqlitePool,
    job_id: &str,
) -> Result<Vec<ProgressiveBatchSegmentRow>, sqlx::Error> {
    sqlx::query_as::<_, ProgressiveBatchSegmentRow>(
        "SELECT * FROM progressive_batch_segments
         WHERE job_id = ? AND status = 'completed'
         ORDER BY segment_index ASC",
    )
    .bind(job_id)
    .fetch_all(pool)
    .await
}

pub async fn mark_interrupted_progressive_batch_jobs(
    pool: &SqlitePool,
) -> Result<Vec<ProgressiveBatchJobRow>, sqlx::Error> {
    // Find all running jobs and mark them interrupted
    let running = sqlx::query_as::<_, ProgressiveBatchJobRow>(
        "SELECT * FROM progressive_batch_jobs WHERE status = 'running'",
    )
    .fetch_all(pool)
    .await?;

    for job in &running {
        sqlx::query(
            "UPDATE progressive_batch_jobs
             SET status = 'interrupted',
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(&job.id)
        .execute(pool)
        .await?;
    }

    Ok(running)
}
