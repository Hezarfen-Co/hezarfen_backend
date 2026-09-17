-- School-database schema, part 6: the RAG nest — its own thread and message
-- tables, and the corpus document id the `rag.index` worker stamps on a
-- course-note file so a later citation can be resolved back to something a
-- reader can open.
--
-- Separate from the chatbot's tables on purpose: a RAG turn is a retrieval
-- over the school's course-note corpus followed by an answer, the service is
-- named on the wire as `rag.chat`, and the stored answer carries its citations
-- as JSON — a shape the chatbot's rows never hold. The two nests are gated as
-- one package (`Module::Chatbot`) but share no row.
--
-- Same translation rules as parts 1-5: entity ids are app-minted UUID v7 (no
-- DB default), timestamps stay BIGINT unix-ms, and all FKs are ON DELETE NO
-- ACTION — except `rag_message.thread`, which cascades: a turn has no life of
-- its own, so even a path that removes the thread without walking the children
-- cannot strand one.

-- The seat counter this nest's threads are capped on, the twin of
-- `chatbot_thread_count` and a column of its own rather than a shared pool:
-- the school's `max_chatbot_threads` governs both nests' saved threads, but
-- each nest holds its own seats, so a user's RAG threads cannot spend the
-- chatbot's room or the reverse — and the counter a claim moves counts exactly
-- rows it guards (the single-record `UPDATE` is the claim; see
-- `db::rag_thread::create_capped`).
ALTER TABLE app_user ADD COLUMN rag_thread_count BIGINT NOT NULL DEFAULT 0;

CREATE TABLE rag_thread (
    id         uuid PRIMARY KEY,
    owner      uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    title      TEXT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

-- The list's sort key, the same shape `chatbot_thread_user_updated` carries.
CREATE INDEX rag_thread_owner_updated ON rag_thread (owner, updated_at);

-- One turn of a RAG thread. `user_id` is duplicated off the thread exactly as
-- `chatbot_message` duplicates it, so an ownership check is one read with no
-- join. `role`/`status` spell their enums in a CHECK for the same reason the
-- chatbot's columns do: the Rust enums are the source, the CHECK is the backstop.
--
-- `content` is `''` on a reserved (pending) assistant row, hence the DEFAULT.
-- `reply` is the service's own JSON — whether it abstained, why, and the
-- citations — NULL until the answer lands; JSONB because its shape belongs to
-- the service, not this schema. There is no `truncated` twin of the chatbot's
-- column: an answer whose citations exceed MAX_RAG_CITATIONS is refused whole
-- (a clipped citation list still renders `[N]` markers into nothing), so no
-- read ever has to ask whether what it got is all there was.
CREATE TABLE rag_message (
    id           uuid PRIMARY KEY,
    thread       uuid NOT NULL REFERENCES rag_thread(id) ON DELETE CASCADE,
    user_id      uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    role         TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    status       TEXT NOT NULL CHECK (status IN ('pending', 'complete', 'failed')),
    content      TEXT NOT NULL DEFAULT '',
    reply        jsonb NULL,
    error_code   TEXT NULL,
    created_at   BIGINT NOT NULL,
    completed_at BIGINT NULL
);

CREATE INDEX rag_message_thread_created ON rag_message (thread, created_at);
CREATE INDEX rag_message_user ON rag_message (user_id);
CREATE INDEX rag_message_status_created ON rag_message (status, created_at);

-- The corpus document id `rag.index` echoed back for this file's bytes: the
-- handle a citation's `doc_id` resolves through to a file. Deliberately not
-- unique — identical bytes hash to one document, so several files may claim
-- the same id and the resolver answers with the first file that owns it.
-- NULL until the worker has indexed the file.
ALTER TABLE course_note_file ADD COLUMN rag_doc_id TEXT;
CREATE INDEX course_note_file_rag_doc_id ON course_note_file (rag_doc_id);
