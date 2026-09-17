//! Wire format for the AI bridge ("hab/2" — hezarfen ai bridge).
//!
//! # Shape
//!
//! The backend is the QUIC **server**: AI services dial in, so a service can
//! live behind NAT, restart freely, and scale by simply opening a second
//! connection. Every service holds exactly one QUIC connection.
//!
//! On that connection:
//!
//! * The **control stream** is the first *client-initiated* bidirectional
//!   stream. The service writes one [`Hello`]; the backend answers one
//!   [`Greeting`] and then leaves the stream open forever. Its death is the
//!   deregistration signal — no heartbeat frame needed.
//! * Each request is one *server-initiated* bidirectional stream: the backend
//!   opens it, writes one [`Request`], finishes its send side, and reads one
//!   [`Response`]. The stream is then closed.
//!
//! * Each **api read** goes the other way: the service opens a *client-initiated*
//!   bidirectional stream (any after the control one), writes one
//!   [`ApiRequest`], finishes its send side, and reads one [`ApiResponse`].
//!   That mirrors the REST API — the backend dispatches the path internally —
//!   so a service asks for school data instead of being handed it.
//!
//! * Each **blob read** goes the same way as an api read, on its own
//!   client-initiated bidirectional stream: the service writes one
//!   [`BlobRequest`], finishes its send side, and reads one [`BlobResponse`]
//!   header frame. On [`BlobResponse::Ok`] exactly `size` **raw** bytes follow
//!   the header — not a frame, so [`AI_MAX_FRAME_BYTES`] does not bound them —
//!   and then the stream is finished. That is how a service gets a course
//!   note's file bytes, which no JSON frame could carry.
//!
//! * Each **capability call** also goes the service's way, on its own
//!   client-initiated bidirectional stream: the service writes one
//!   [`CapabilityRequest`], finishes its send side, and reads one
//!   [`CapabilityResponse`]. This is the reverse of the [`Request`]/
//!   [`Response`] pair above, and it exists for the same reason the api read
//!   does: capabilities live on both sides. The backend *dispatches*
//!   `chat.reply` and `insight.student` to a service; a service *calls*
//!   `insight.summary.upsert` and the rest of the backend's storage surface
//!   to have the backend write rows an AI service may not write itself.
//!
//!   The three client-initiated request shapes are told apart by their
//!   required field: an [`ApiRequest`] has `path`, a [`BlobRequest`] has
//!   `file`, a [`CapabilityRequest`] has `capability`.
//!
//! * Each **blob upload** is the blob read's mirror image: the service writes
//!   one [`BlobUploadRequest`], then exactly `size` **raw** bytes — again not a
//!   frame — and finishes its send side; the backend stores them and answers
//!   one [`BlobUploadResponse`] header frame. That is how a service hands back
//!   an artifact it produced (today the podcast service's finished mp3), which
//!   no JSON frame could carry. Distinguished from a [`BlobRequest`] by its
//!   required `upload` marker and `job_id`.
//!
//! # Refusal codes
//!
//! [`ApiResponse::Err`], [`BlobResponse::Err`] and [`CapabilityResponse::Err`]
//! all carry a `code`: a stable, machine-readable string a service branches
//! on, never a message. The vocabulary, in one place:
//!
//! * `malformed` — the frame is not this shape (a missing or mistyped field).
//! * `unknown_capability` — no operation this backend serves has that name.
//! * `unknown_school` — the named slug is not a school on this deployment.
//! * `school_suspended` — it exists and is switched off; worth retrying later.
//! * `not_permitted` — the school's own feature set refuses the operation.
//! * `invalid_payload` — the payload does not fit the operation's contract
//!   (`message` names the field).
//! * `too_many_rows` — a count bound was exceeded (`message` names it); the
//!   operation was refused whole, never applied in part.
//! * `unavailable` — the school's database could not be reached.
//! * `timed_out` — the operation outlived the bridge's deadline; whether it
//!   landed is unknown, so retrying is the caller's decision.
//! * `internal` — the backend failed while doing the work.
//! * `path_not_allowed`, `unknown_user`, `method_not_allowed` — api-read only.
//!
//! # School scoping
//!
//! Every request frame names its school by slug, and every answer echoes it:
//! [`Request::school`], [`ApiRequest::school`], [`BlobRequest::school`] and
//! their responses. The AI fleet is *shared* across the deployment — one
//! service serves every school — so the school cannot be bound once at
//! handshake time and [`Hello`] deliberately carries none: a service pinned to
//! one school would have to be run once per customer. A frame without a
//! `school` is `malformed`, an unknown slug is `unknown_school`, a suspended
//! one `school_suspended`; there is no default and no fallback, because a read
//! answered out of the wrong school's database is the one failure this field
//! exists to make impossible.
//!
//! There is deliberately no correlation-id matching: QUIC stream IDs already
//! multiplex concurrent requests over the one connection, independently
//! flow-controlled, with no head-of-line blocking between them. [`Request::id`]
//! is a trace id for logs on both sides, nothing more.
//!
//! # Framing
//!
//! `u32` big-endian byte length, then that many bytes of JSON. JSON (not a
//! compact binary codec) because the AI services are expected to be written in
//! whatever language suits the model, and a length-prefixed JSON frame is
//! twenty lines in any of them.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::constant::{AI_MAX_FRAME_BYTES, AI_PROTOCOL};

/// Why a frame could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("stream ended before a complete frame arrived")]
    Eof,
    #[error("frame of {0} bytes exceeds the {AI_MAX_FRAME_BYTES}-byte limit")]
    TooLarge(u32),
    #[error("frame was not valid JSON for the expected message: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("stream i/o failed: {0}")]
    Io(#[source] std::io::Error),
}

/// The service's opening frame on the control stream.
///
/// Carries **no** school on purpose: the fleet is shared, so one connection
/// serves every school on the deployment and each frame names its own (see
/// this module's "School scoping").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    /// Must equal [`crate::constant::AI_PROTOCOL`]; anything else is rejected
    /// rather than guessed at.
    pub protocol: String,
    /// Human-readable service name, for logs (`"ocr"`, `"grader"`).
    pub service: String,
    /// The capability strings this service will answer, e.g.
    /// `["ocr.extract", "ocr.detect_lang"]`. Requests are routed by exact
    /// match on one of these.
    pub capabilities: Vec<String>,
    /// Shared secret, compared against `AI_SHARED_TOKEN` in constant time.
    pub token: String,
    /// How many requests this worker will accept at once. Clamped to
    /// `1..=`[`crate::constant::AI_MAX_CONCURRENT_PER_WORKER`]; absent means
    /// the default.
    #[serde(default)]
    pub max_concurrent: Option<u32>,
}

/// The backend's answer on the control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Greeting {
    Welcome {
        /// Server-assigned worker id, echoed in the backend's logs so a
        /// service can correlate its own logs with ours.
        worker_id: String,
        protocol: String,
    },
    Rejected {
        code: RejectCode,
        message: String,
    },
}

/// Why a handshake was refused. The service should not retry `Unauthorized`
/// or `UnsupportedProtocol` without a config change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    UnsupportedProtocol,
    Unauthorized,
    NoCapabilities,
    Malformed,
}

/// One unit of work, written by the backend on a fresh server-initiated
/// stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Trace id (ULID). Not used for correlation — the stream does that.
    pub id: String,
    /// Slug of the school this work belongs to. Required — see this module's
    /// "School scoping".
    pub school: String,
    /// Which capability from the worker's [`Hello::capabilities`] to invoke.
    pub capability: String,
    /// How long the backend will wait before abandoning the stream. The
    /// service should stop work rather than answer late.
    pub deadline_ms: u64,
    /// Capability-specific body, opaque to the transport.
    pub payload: Value,
}

/// The service's single answer frame. `Err` is a *handled* failure (bad input,
/// model refused); a service that dies mid-request just drops the stream,
/// which surfaces as a transport error instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok {
        id: String,
        /// Echo of [`Request::school`].
        school: String,
        payload: Value,
    },
    Err {
        id: String,
        /// Echo of [`Request::school`].
        school: String,
        /// Service-defined, stable, machine-readable (`"unsupported_image"`).
        code: String,
        message: String,
    },
}

/// A read of the school's own API, written by the *service* on a fresh
/// client-initiated stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiRequest {
    /// Trace id (ULID). Not used for correlation — the stream does that.
    pub id: String,
    /// Slug of the school to read. Required — see this module's "School
    /// scoping"; the answer comes out of that school's own database.
    pub school: String,
    /// Path as the REST API spells it, e.g. `"/users/me"`. No host, no query.
    pub path: String,
    /// Query string without the leading `?`, e.g. `"limit=10&offset=0"`.
    #[serde(default)]
    pub query: Option<String>,
    /// User id to execute as, for own-scoped endpoints. Absent means the
    /// request runs as the service itself.
    #[serde(default)]
    pub on_behalf_of: Option<String>,
    /// HTTP method. Absent means `GET`. Not validated here — the server
    /// decides what it will dispatch.
    #[serde(default)]
    pub method: Option<String>,
}

/// The backend's single answer frame. `Err` is a bridge-level refusal (path not
/// allowed, unknown user); an API call that ran and answered `404` is an `Ok`
/// carrying that status, because the service asked and the API replied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ApiResponse {
    Ok {
        id: String,
        /// Echo of [`ApiRequest::school`].
        school: String,
        /// HTTP status the router produced.
        status: u16,
        body: Value,
    },
    Err {
        id: String,
        /// Echo of the school the frame named — as sent, even when it named no
        /// school a slug could be made of, so a service can tell which of its
        /// in-flight reads was refused.
        school: String,
        /// Bridge-defined, stable, machine-readable (`"path_not_allowed"`,
        /// `"unknown_school"`, `"school_suspended"`).
        code: String,
        message: String,
    },
}

/// A read of one course-note file's *bytes*, written by the *service* on a
/// fresh client-initiated stream. Distinguished from an [`ApiRequest`] by its
/// required `file` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlobRequest {
    /// Trace id (ULID). Not used for correlation — the stream does that.
    pub id: String,
    /// Slug of the school the file belongs to. Required — see this module's
    /// "School scoping"; the bytes come out of that school's blob directory.
    pub school: String,
    /// The `course_note_file` record key, as `GET /course-notes/{id}/files`
    /// and the `rag.index` payload both publish it.
    pub file: String,
    /// User id to read as. Absent means the service itself, which can view no
    /// course and so always earns `forbidden`.
    #[serde(default)]
    pub on_behalf_of: Option<String>,
}

/// The header frame that opens (or refuses) a blob stream. On `Ok` exactly
/// `size` raw bytes follow it, then FIN; on `Err` the stream is finished with
/// nothing after the frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BlobResponse {
    Ok {
        id: String,
        /// Echo of [`BlobRequest::school`].
        school: String,
        name: String,
        content_type: String,
        /// Exactly how many raw bytes follow this frame.
        size: u64,
    },
    Err {
        id: String,
        /// Echo of the school the frame named, as sent.
        school: String,
        /// Bridge-defined, stable, machine-readable (`"not_found"`).
        code: String,
        message: String,
    },
}

/// A call of one capability the *backend* serves, written by the service on a
/// fresh client-initiated stream. The mirror image of [`Request`], which the
/// backend writes when it calls a capability the *service* serves.
///
/// Distinguished from an [`ApiRequest`] by its required `capability` field and
/// from a [`BlobRequest`] by its required `school` and `payload`. There is no
/// path and no method: an operation is named, never addressed — the backend's
/// storage surface is a list of operations, and a frame that could name a
/// table, a schema or a query would be the door this shape exists to not have.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRequest {
    /// Trace id (ULID). Not used for correlation — the stream does that.
    pub id: String,
    /// Slug of the school the call is scoped to. Required, exactly as on
    /// every other frame — one capability is deployment-scoped
    /// (`insight.schools.list`, whose answer *is* the school directory) and
    /// sends the empty string there.
    pub school: String,
    /// The operation to run. Matched exactly against the backend's served
    /// capabilities; anything else is `unknown_capability`.
    pub capability: String,
    /// Operation-specific body, opaque to the transport.
    pub payload: Value,
}

/// The backend's single answer frame, mirroring [`Response`] field for field:
/// `status` is the discriminator, and both variants echo the school the call
/// named so a service can tell which of its in-flight calls was answered.
///
/// `Err` is a *handled* refusal (unknown capability, a payload that does not
/// fit, the school's database unreachable). A service that dies mid-call just
/// drops the stream, which surfaces as a transport error instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CapabilityResponse {
    Ok {
        /// Echo of [`CapabilityRequest::id`].
        id: String,
        /// Echo of [`CapabilityRequest::school`].
        school: String,
        payload: Value,
    },
    Err {
        /// Echo of [`CapabilityRequest::id`].
        id: String,
        /// Echo of [`CapabilityRequest::school`], as sent.
        school: String,
        /// One of this module's documented refusal codes.
        code: String,
        message: String,
    },
}

/// A write of one produced artifact's *bytes*, written by the *service* on a
/// fresh client-initiated stream. The mirror image of [`BlobRequest`]: the
/// service names what it is handing over, exactly `size` raw bytes follow this
/// frame, and the backend stores them somewhere that belongs to the school —
/// never to the service's own volume.
///
/// Distinguished from a [`BlobRequest`] (the read) by the required `upload`
/// marker and its `job_id`: an upload is always *about* one backend-minted
/// job, and the backend links the stored blob to that job's row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlobUploadRequest {
    /// Trace id (ULID). Not used for correlation — the stream does that.
    pub id: String,
    /// The shape marker. Must be `true`; any other value is `malformed`.
    pub upload: bool,
    /// Slug of the school the artifact belongs to. Required — the bytes land
    /// in that school's own blob directory, never anywhere else.
    pub school: String,
    /// The backend-minted podcast job the artifact belongs to.
    pub job_id: String,
    /// The artifact's display name (an episode's file name). Stored as
    /// metadata; never used to build a path.
    pub name: String,
    /// The artifact's MIME type. Must be an `audio/*` type today.
    pub content_type: String,
    /// Exactly how many raw bytes follow this frame.
    pub size: u64,
    /// The episode's length, when the producer knows it. Metadata for the
    /// read doors; `None` is allowed.
    #[serde(default)]
    pub duration_secs: Option<f64>,
}

/// The single answer frame an upload gets. There are no bytes after it: the
/// artifact travels service → backend only, and `Ok.key` is the handle the
/// read doors publish (`GET /podcast/jobs/{id}/audio` resolves it). A refusal
/// carries nothing after the frame, and the backend resets the stream rather
/// than reading a body it will not store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BlobUploadResponse {
    Ok {
        /// Echo of [`BlobUploadRequest::id`].
        id: String,
        /// Echo of [`BlobUploadRequest::school`].
        school: String,
        /// The key the bytes were stored under, relative to the school's blob
        /// root (`podcast/<job-id>.<ext>`).
        key: String,
        /// How many bytes were written — always the request's `size`.
        size: u64,
    },
    Err {
        /// Echo of [`BlobUploadRequest::id`].
        id: String,
        /// Echo of [`BlobUploadRequest::school`], as sent.
        school: String,
        /// This module's documented refusal codes, plus the operation's own
        /// (`unknown_job`, `expired`, `audio_missing`, …) — flat and stable,
        /// never nested.
        code: String,
        message: String,
    },
}

/// Write one length-prefixed JSON frame.
pub async fn write_frame<W, T>(w: &mut W, message: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(message).map_err(FrameError::Malformed)?;
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if body.len() > AI_MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    w.write_all(&len.to_be_bytes())
        .await
        .map_err(FrameError::Io)?;
    w.write_all(&body).await.map_err(FrameError::Io)?;
    w.flush().await.map_err(FrameError::Io)?;
    Ok(())
}

/// Read one length-prefixed JSON frame.
///
/// The length is checked *before* allocating, so a hostile or confused peer
/// cannot make the backend reserve gigabytes by lying in four bytes.
pub async fn read_frame<R, T>(r: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut header = [0u8; 4];
    read_exact(r, &mut header).await?;
    let len = u32::from_be_bytes(header);
    if len as usize > AI_MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    read_exact(r, &mut body).await?;
    serde_json::from_slice(&body).map_err(FrameError::Malformed)
}

/// `read_exact` that reports a clean peer FIN as [`FrameError::Eof`] rather
/// than an opaque io error — a service closing its control stream is normal.
async fn read_exact<R: AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> Result<(), FrameError> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]).await {
            Ok(0) => return Err(FrameError::Eof),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FrameError::Eof),
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    Ok(())
}

/// Does this [`Hello`] speak our protocol version?
pub fn protocol_matches(hello: &Hello) -> bool {
    hello.protocol == AI_PROTOCOL
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hello() -> Hello {
        Hello {
            protocol: AI_PROTOCOL.to_string(),
            service: "ocr".into(),
            capabilities: vec!["ocr.extract".into()],
            token: "secret".into(),
            max_concurrent: Some(4),
        }
    }

    #[tokio::test]
    async fn frames_round_trip_through_a_pipe() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let sent = hello();
        write_frame(&mut client, &sent).await.unwrap();
        let got: Hello = read_frame(&mut server).await.unwrap();
        assert_eq!(got, sent);
    }

    #[tokio::test]
    async fn back_to_back_frames_stay_aligned() {
        // The length prefix, not the stream boundary, delimits a frame — two
        // writes must read back as exactly two messages.
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let a = Response::Ok {
            id: "a".into(),
            school: "demo".into(),
            payload: json!({ "n": 1 }),
        };
        let b = Response::Err {
            id: "b".into(),
            school: "demo".into(),
            code: "bad_input".into(),
            message: "nope".into(),
        };
        write_frame(&mut client, &a).await.unwrap();
        write_frame(&mut client, &b).await.unwrap();
        assert_eq!(read_frame::<_, Response>(&mut server).await.unwrap(), a);
        assert_eq!(read_frame::<_, Response>(&mut server).await.unwrap(), b);
    }

    #[tokio::test]
    async fn a_clean_close_reads_as_eof_not_an_io_error() {
        let (client, mut server) = tokio::io::duplex(64);
        drop(client);
        let err = read_frame::<_, Hello>(&mut server).await.unwrap_err();
        assert!(matches!(err, FrameError::Eof), "got {err:?}");
    }

    #[tokio::test]
    async fn a_truncated_frame_is_eof_not_a_parse_of_partial_json() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        client.write_all(&8u32.to_be_bytes()).await.unwrap();
        client.write_all(b"{\"a\":").await.unwrap();
        drop(client);
        assert!(matches!(
            read_frame::<_, Hello>(&mut server).await.unwrap_err(),
            FrameError::Eof
        ));
    }

    #[tokio::test]
    async fn an_oversized_length_is_refused_before_allocating() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        // Only the four-byte header is ever written: the reader must refuse on
        // the header alone, without waiting for (or reserving) the body.
        let claimed = AI_MAX_FRAME_BYTES as u32 + 1;
        client.write_all(&claimed.to_be_bytes()).await.unwrap();
        let err = read_frame::<_, Hello>(&mut server).await.unwrap_err();
        assert!(
            matches!(err, FrameError::TooLarge(n) if n == claimed),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn garbage_body_is_malformed_not_a_panic() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let body = b"not json at all";
        client
            .write_all(&(body.len() as u32).to_be_bytes())
            .await
            .unwrap();
        client.write_all(body).await.unwrap();
        assert!(matches!(
            read_frame::<_, Hello>(&mut server).await.unwrap_err(),
            FrameError::Malformed(_)
        ));
    }

    #[tokio::test]
    async fn greeting_and_response_tags_are_the_documented_wire_names() {
        // Services in other languages match on these literals — a rename here
        // silently breaks every one of them, so pin the encoding.
        let welcome = serde_json::to_value(Greeting::Welcome {
            worker_id: "w1".into(),
            protocol: AI_PROTOCOL.into(),
        })
        .unwrap();
        assert_eq!(welcome["type"], "welcome");
        let rejected = serde_json::to_value(Greeting::Rejected {
            code: RejectCode::Unauthorized,
            message: "bad token".into(),
        })
        .unwrap();
        assert_eq!(rejected["type"], "rejected");
        assert_eq!(rejected["code"], "unauthorized");
        let ok = serde_json::to_value(Response::Ok {
            id: "1".into(),
            school: "demo".into(),
            payload: json!(null),
        })
        .unwrap();
        assert_eq!(ok["status"], "ok");
    }

    #[tokio::test]
    async fn api_frame_tags_are_the_documented_wire_names() {
        // Same reason as above: other-language services match these literals.
        // The tag is `outcome`, not `status` — `status` is the HTTP code.
        let ok = serde_json::to_value(ApiResponse::Ok {
            id: "1".into(),
            school: "demo".into(),
            status: 404,
            body: json!(null),
        })
        .unwrap();
        assert_eq!(ok["outcome"], "ok");
        assert_eq!(ok["status"], 404);
        let err = serde_json::to_value(ApiResponse::Err {
            id: "1".into(),
            school: "demo".into(),
            code: "path_not_allowed".into(),
            message: "nope".into(),
        })
        .unwrap();
        assert_eq!(err["outcome"], "err");
        assert_eq!(err["code"], "path_not_allowed");
    }

    #[tokio::test]
    async fn capability_frame_tags_are_the_documented_wire_names() {
        // The answer mirrors `Response` exactly — same `status` discriminator,
        // same field names — so a service decodes both with one function.
        // Pinned because other-language services match these literals.
        let ok = serde_json::to_value(CapabilityResponse::Ok {
            id: "1".into(),
            school: "demo".into(),
            payload: json!({ "written": 2 }),
        })
        .unwrap();
        assert_eq!(ok["status"], "ok");
        assert_eq!(ok["id"], "1");
        assert_eq!(ok["school"], "demo");
        assert_eq!(ok["payload"]["written"], 2);
        let err = serde_json::to_value(CapabilityResponse::Err {
            id: "1".into(),
            school: "demo".into(),
            code: "unknown_capability".into(),
            message: "nope".into(),
        })
        .unwrap();
        assert_eq!(err["status"], "err");
        assert_eq!(err["code"], "unknown_capability");
        assert_eq!(err["school"], "demo");
        // A refusal carries no payload: there is no half-answer to read.
        assert!(err.get("payload").is_none());
    }

    #[tokio::test]
    async fn a_capability_request_needs_an_id_a_school_a_capability_and_a_payload() {
        let call: CapabilityRequest = serde_json::from_value(json!({
            "id": "01J",
            "school": "demo",
            "capability": "insight.summary.upsert",
            "payload": { "rows": [] },
        }))
        .unwrap();
        assert_eq!(call.capability, "insight.summary.upsert");
        // Each of the three fields is required: a frame missing its
        // capability names no operation to run, and a frame missing its
        // school could only be answered out of a database nobody named.
        for missing in ["id", "school", "capability", "payload"] {
            let mut frame = json!({
                "id": "01J",
                "school": "demo",
                "capability": "insight.pending.list",
                "payload": {},
            });
            frame.as_object_mut().unwrap().remove(missing);
            assert!(
                serde_json::from_value::<CapabilityRequest>(frame).is_err(),
                "a frame without `{missing}` must not parse"
            );
        }
    }

    #[tokio::test]
    async fn the_client_initiated_shapes_do_not_parse_as_each_other() {
        // Routing is by required field (`path` / `file` / `capability`), so
        // this is the property that keeps a capability call from being
        // mistaken for an api read — or for a blob read, which is what the
        // frame would look like if `capability` were ever optional.
        let call = json!({
            "id": "01J",
            "school": "demo",
            "capability": "insight.pending.list",
            "payload": {},
        });
        assert!(
            serde_json::from_value::<CapabilityRequest>(call.clone()).is_ok(),
            "the capability frame must parse as a capability call"
        );
        assert!(
            serde_json::from_value::<ApiRequest>(call.clone()).is_err(),
            "a capability call must not parse as an api read"
        );
        assert!(
            serde_json::from_value::<BlobRequest>(call).is_err(),
            "a capability call must not parse as a blob read"
        );
    }

    #[tokio::test]
    async fn an_api_request_needs_only_an_id_a_school_and_a_path() {
        let bare: ApiRequest = serde_json::from_value(json!({
            "id": "01J",
            "school": "demo",
            "path": "/users/me",
        }))
        .unwrap();
        assert_eq!(bare.query, None);
        assert_eq!(bare.on_behalf_of, None);
        assert_eq!(bare.method, None);
        // The school is required, not defaulted: a frame that names none has
        // no school to fall back on and must not parse.
        assert!(
            serde_json::from_value::<ApiRequest>(json!({ "id": "01J", "path": "/users/me" }))
                .is_err()
        );

        let full = ApiRequest {
            id: "01J".into(),
            school: "demo".into(),
            path: "/notes".into(),
            query: Some("limit=10".into()),
            on_behalf_of: Some("user:abc".into()),
            // Not validated at this layer — any string rides the wire.
            method: Some("GET".into()),
        };
        let raw = serde_json::to_value(&full).unwrap();
        assert_eq!(raw["school"], "demo");
        assert_eq!(raw["path"], "/notes");
        assert_eq!(raw["query"], "limit=10");
        assert_eq!(raw["on_behalf_of"], "user:abc");
        assert_eq!(raw["method"], "GET");
        assert_eq!(serde_json::from_value::<ApiRequest>(raw).unwrap(), full);
    }

    #[tokio::test]
    async fn blob_frame_tags_are_the_documented_wire_names() {
        // Same reason as above: other-language services match these literals.
        // The tag is `status` (as on `Response`), not `outcome` — a blob header
        // carries no HTTP status for it to collide with.
        let ok = serde_json::to_value(BlobResponse::Ok {
            id: "1".into(),
            school: "demo".into(),
            name: "recap.pdf".into(),
            content_type: "application/pdf".into(),
            size: 204_800,
        })
        .unwrap();
        assert_eq!(ok["status"], "ok");
        assert_eq!(ok["size"], 204_800);
        let err = serde_json::to_value(BlobResponse::Err {
            id: "1".into(),
            school: "demo".into(),
            code: "not_found".into(),
            message: "nope".into(),
        })
        .unwrap();
        assert_eq!(err["status"], "err");
        assert_eq!(err["code"], "not_found");

        // `file` is what tells a blob request apart from an api read, so it is
        // required; `on_behalf_of` is the only optional field.
        let bare: BlobRequest = serde_json::from_value(json!({
            "id": "01J", "school": "demo", "file": "01FILE",
        }))
        .unwrap();
        assert_eq!(bare.on_behalf_of, None);
        assert!(
            serde_json::from_value::<BlobRequest>(json!({ "id": "01J", "school": "demo" }))
                .is_err()
        );
        // The school is required here too.
        assert!(
            serde_json::from_value::<BlobRequest>(json!({ "id": "01J", "file": "01FILE" }))
                .is_err()
        );
        // And the two shapes never parse as each other.
        assert!(
            serde_json::from_value::<ApiRequest>(json!({
                "id": "01J", "school": "demo", "file": "01FILE",
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn max_concurrent_is_optional_on_the_wire() {
        let raw = json!({
            "protocol": AI_PROTOCOL,
            "service": "ocr",
            "capabilities": ["ocr.extract"],
            "token": "secret",
        });
        let hello: Hello = serde_json::from_value(raw).unwrap();
        assert_eq!(hello.max_concurrent, None);
        assert!(protocol_matches(&hello));
    }

    #[tokio::test]
    async fn a_foreign_protocol_version_does_not_match() {
        let mut h = hello();
        h.protocol = "hab/99".into();
        assert!(!protocol_matches(&h));
    }
}
