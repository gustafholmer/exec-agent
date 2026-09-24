//! `attach_receipt` — upload a receipt file to Fortnox and bind it to a
//! voucher.
//!
//! Two POSTs, in this order and only this order:
//!
//! 1. `POST inbox?path=Inbox_v` with the file as `multipart/form-data`, which
//!    answers with `File.Id`. This is the only multipart request in the
//!    workspace, and the reason [`FortnoxClient::post_form`] exists: a
//!    `reqwest::multipart::Form` is consumed by the send, so the client's
//!    bounded 401 retry rebuilds the body from a description rather than
//!    re-sending a spent one.
//! 2. `POST voucherfileconnections?financialyear=<year>` naming that file id
//!    and the voucher.
//!
//! Ported from `attachTools.ts` plus `fortnox/files.ts`, which upstream keeps
//! in its core package. There is no `confirm` here either, for the reason in
//! [`super::write`].
//!
//! # What is checked before anything leaves the machine
//!
//! The extension must be one Fortnox accepts, and the file must be readable.
//! Both are refused locally, so a typo costs no request. The *bytes* are a
//! customer's receipt: they never reach a log, an error or a `Debug` — see
//! [`ea_fortnox::client::FormPart`], whose `Debug` prints a length and
//! `<redacted>`.

use std::path::Path;

use ea_fortnox::client::FormPart;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::Value;

use super::{render, FortnoxServer};

/// Extensions Fortnox's archive accepts. Upstream's set.
pub const ALLOWED_EXTENSIONS: &[&str] = &["pdf", "tif", "tiff", "jpg", "jpeg"];

/// The Fortnox inbox folder vouchers' files live in.
const INBOX_FOLDER: &str = "Inbox_v";

/// The largest receipt this will upload, in bytes.
///
/// Not upstream's — upstream has no limit. A receipt is a page or two of PDF
/// or a phone photo; ten megabytes is far above any of those and far below
/// what would make the daemon's per-call deadline the thing that fails. The
/// point is to refuse a mistake (a whole scanned archive, a video) with a
/// message that says what happened, rather than to time out mid-upload and
/// leave a half-attached file behind.
pub const MAX_RECEIPT_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct AttachReceiptArgs {
    /// Absolute path to the receipt file on this machine. PDF, TIF or JPG.
    pub file_path: String,
    /// Voucher series, e.g. "A" — as returned by record_expense.
    pub series: String,
    /// Voucher number, as returned by record_expense.
    pub number: i64,
    /// Fortnox financial-year id — the voucher's Year, as returned by
    /// record_expense.
    pub financial_year: i64,
}

#[tool_router(router = attach_router, vis = "pub(crate)")]
impl FortnoxServer {
    #[tool(
        description = "Upload a local receipt or invoice file (PDF, TIF or JPG) to Fortnox \
                       and attach it to a voucher that already exists. Use the series, \
                       number and financial_year that record_expense returned. An \
                       unsupported file type or an unreadable path is refused without \
                       anything being uploaded. Posts immediately. Reachable only through \
                       propose_action; the approval gate decides whether it runs."
    )]
    pub async fn attach_receipt(
        &self,
        Parameters(AttachReceiptArgs {
            file_path,
            series,
            number,
            financial_year,
        }): Parameters<AttachReceiptArgs>,
    ) -> Result<String, String> {
        let path = Path::new(&file_path);
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if !ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
            let named = if extension.is_empty() {
                "(none)".to_string()
            } else {
                extension
            };
            return Err(format!(
                "fortnox: unsupported file type \"{named}\". Fortnox accepts PDF, TIF or JPG."
            ));
        }
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("fortnox: {file_path} has no file name."))?
            .to_string();

        // Size first, so a mistake is refused without being read into memory.
        // The error names the path, never the contents.
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() > MAX_RECEIPT_BYTES => {
                return Err(format!(
                    "fortnox: {file_path} is {} bytes; a receipt must be at most {} bytes.",
                    meta.len(),
                    MAX_RECEIPT_BYTES
                ));
            }
            Ok(_) => {}
            Err(_) => return Err(format!("fortnox: could not read file at {file_path}.")),
        }
        let bytes = std::fs::read(path)
            .map_err(|_| format!("fortnox: could not read file at {file_path}."))?;

        let client = self.client()?;

        let uploaded = client
            .post_form(
                "inbox",
                &[FormPart {
                    name: "file",
                    bytes: &bytes,
                    filename: Some(&filename),
                    mime: None,
                }],
                &[("path", INBOX_FOLDER)],
            )
            .await
            .map_err(render)?;

        let file_id = uploaded
            .get("File")
            .and_then(|file| file.get("Id"))
            .and_then(scalar)
            .ok_or_else(|| "fortnox: the inbox upload returned no file id.".to_string())?;

        let year = financial_year.to_string();
        client
            .post(
                "voucherfileconnections",
                &serde_json::json!({
                    "VoucherFileConnection": {
                        "FileId": file_id,
                        "VoucherSeries": series,
                        "VoucherNumber": number,
                    },
                }),
                &[("financialyear", year.as_str())],
            )
            .await
            .map_err(render)?;

        Ok(format!(
            "Attached {filename} (file {file_id}) to voucher {series}{number}."
        ))
    }
}

/// Fortnox's file ids are strings, but the API has been known to answer with a
/// number; either is a usable id.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{header_exists, method, path as path_matcher, query_param};
    use wiremock::{Mock, MockServer};

    use crate::tools::test_support::{json_body, server_for};

    fn args(file_path: &str) -> Parameters<AttachReceiptArgs> {
        Parameters(AttachReceiptArgs {
            file_path: file_path.to_string(),
            series: "A".to_string(),
            number: 7,
            financial_year: 2,
        })
    }

    async fn fortnox_accepting_uploads() -> MockServer {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/inbox"))
            .and(query_param("path", "Inbox_v"))
            .and(header_exists("content-type"))
            .respond_with(json_body(serde_json::json!({
                "File": { "Id": "file-9", "Name": "receipt.pdf" }
            })))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path_matcher("/voucherfileconnections"))
            .and(query_param("financialyear", "2"))
            .respond_with(json_body(serde_json::json!({
                "VoucherFileConnection": { "FileId": "file-9" }
            })))
            .mount(&mock)
            .await;
        mock
    }

    fn receipt(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("writing the fixture receipt");
        path.to_string_lossy().into_owned()
    }

    /// The multipart path end to end: this is why `post_form` exists.
    #[tokio::test]
    async fn attach_receipt_uploads_multipart_then_connects_the_file_to_the_voucher() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = receipt(&dir, "receipt.pdf", &[1, 2, 3]);
        let mock = fortnox_accepting_uploads().await;

        let text = server_for(&mock)
            .attach_receipt(args(&file_path))
            .await
            .expect("the upload and the connection both succeed");

        assert!(text.contains("Attached receipt.pdf"), "{text}");
        assert!(text.contains("file-9"), "{text}");
        assert!(text.contains("voucher A7"), "{text}");

        let requests = mock.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "one upload, one connection");

        let upload = &requests[0];
        assert_eq!(upload.url.path(), "/inbox");
        let content_type = upload
            .headers
            .get("content-type")
            .expect("a content type")
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            content_type.starts_with("multipart/form-data"),
            "the inbox upload must be multipart, not JSON: {content_type}"
        );
        assert!(
            content_type.contains("boundary="),
            "reqwest must have set a boundary: {content_type}"
        );
        let body = String::from_utf8_lossy(&upload.body);
        assert!(body.contains("name=\"file\""), "{body}");
        assert!(body.contains("filename=\"receipt.pdf\""), "{body}");

        let connect = &requests[1];
        assert_eq!(connect.url.path(), "/voucherfileconnections");
        let sent: Value = serde_json::from_slice(&connect.body).expect("a JSON body");
        assert_eq!(
            sent,
            serde_json::json!({
                "VoucherFileConnection": {
                    "FileId": "file-9",
                    "VoucherSeries": "A",
                    "VoucherNumber": 7,
                },
            })
        );
    }

    #[tokio::test]
    async fn an_unsupported_extension_is_refused_before_any_upload() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = receipt(&dir, "receipt.txt", b"hi");
        let mock = fortnox_accepting_uploads().await;

        let err = server_for(&mock)
            .attach_receipt(args(&file_path))
            .await
            .unwrap_err();

        assert!(err.contains("PDF, TIF or JPG"), "{err}");
        assert!(mock.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_extensionless_path_says_so_rather_than_naming_an_empty_type() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = receipt(&dir, "receipt", b"hi");
        let mock = fortnox_accepting_uploads().await;
        let err = server_for(&mock)
            .attach_receipt(args(&file_path))
            .await
            .unwrap_err();
        assert!(err.contains("\"(none)\""), "{err}");
        assert!(mock.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_file_is_refused_before_any_upload() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir
            .path()
            .join("missing.pdf")
            .to_string_lossy()
            .into_owned();
        let mock = fortnox_accepting_uploads().await;

        let err = server_for(&mock)
            .attach_receipt(args(&file_path))
            .await
            .unwrap_err();

        assert!(err.contains("could not read file"), "{err}");
        assert!(mock.received_requests().await.unwrap().is_empty());
    }

    /// Case does not matter: a phone writes `IMG_1234.JPG`.
    #[tokio::test]
    async fn an_uppercase_extension_is_accepted() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = receipt(&dir, "IMG_1234.JPG", &[0xff, 0xd8]);
        let mock = fortnox_accepting_uploads().await;
        server_for(&mock)
            .attach_receipt(args(&file_path))
            .await
            .expect("JPG is JPG");
        assert_eq!(mock.received_requests().await.unwrap().len(), 2);
    }

    /// An upload that answers without a file id must not go on to connect
    /// nothing to the voucher.
    #[tokio::test]
    async fn an_upload_with_no_file_id_stops_before_the_connection() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = receipt(&dir, "receipt.pdf", &[1]);
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/inbox"))
            .respond_with(json_body(serde_json::json!({})))
            .mount(&mock)
            .await;

        let err = server_for(&mock)
            .attach_receipt(args(&file_path))
            .await
            .unwrap_err();

        assert!(err.contains("no file id"), "{err}");
        let requests = mock.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "the connection must not be attempted");
    }

    #[tokio::test]
    async fn an_oversized_file_is_refused_before_any_upload() {
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = receipt(
            &dir,
            "huge.pdf",
            &vec![0u8; (MAX_RECEIPT_BYTES + 1) as usize],
        );
        let mock = fortnox_accepting_uploads().await;
        let err = server_for(&mock)
            .attach_receipt(args(&file_path))
            .await
            .unwrap_err();
        assert!(err.contains("at most"), "{err}");
        assert!(mock.received_requests().await.unwrap().is_empty());
    }
}
