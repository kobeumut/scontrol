//! A small in-process S3 client: AWS Signature V4 over `reqwest`, path-style
//! addressing, and exactly the calls backups need (multipart upload, ranged
//! listing, delete, streamed download). It works against AWS S3, Cloudflare
//! R2, and MinIO.
//!
//! Nothing here touches the disk: an upload reads from any `AsyncRead` in
//! `PART_SIZE` chunks, so memory use is bounded by one part regardless of
//! the dump size. Secrets never reach a log line or an error message: the
//! signing key lives in [`S3Client`], whose `Debug` is redacted, and
//! [`redact`] masks any configured secret in text that is surfaced.

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use panel_core::adapters::{BackupObject, S3Target};
use panel_core::error::{CoreError, CoreResult};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Parts are at least 5 MiB (the S3 minimum for all but the last part).
pub const PART_SIZE: usize = 8 * 1024 * 1024;
const MAX_PARTS: u32 = 10_000;
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

type HmacSha256 = Hmac<Sha256>;

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Replaces every secret in `text` with `********`.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for secret in secrets.iter().filter(|s| s.len() >= 4) {
        out = out.replace(secret, "********");
    }
    out
}

/// RFC 3986 percent-encoding as SigV4 wants it (`/` kept when asked).
fn uri_encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn canonical_query(query: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// The three headers SigV4 adds, computed for one request.
pub struct Signed {
    pub authorization: String,
    pub amz_date: String,
    pub content_sha256: String,
}

/// Signs a request with `host`, `x-amz-content-sha256`, and `x-amz-date`.
#[allow(clippy::too_many_arguments)]
pub fn sign(
    method: &str,
    host: &str,
    path: &str,
    query: &[(&str, &str)],
    payload_sha256: &str,
    now: DateTime<Utc>,
    region: &str,
    access_key: &str,
    secret_key: &str,
) -> Signed {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = &amz_date[..8];
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_sha256}\nx-amz-date:{amz_date}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "{method}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_sha256}",
        uri_encode(path, true),
        canonical_query(query),
    );
    let scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));
    Signed {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
        ),
        amz_date,
        content_sha256: payload_sha256.to_string(),
    }
}

pub struct S3Client {
    http: reqwest::Client,
    target: S3Target,
    host: String,
}

impl std::fmt::Debug for S3Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Client")
            .field("endpoint", &self.target.endpoint)
            .field("bucket", &self.target.bucket)
            .finish_non_exhaustive()
    }
}

/// Text between `<tag>` and `</tag>` starting at `from`.
fn tag_value<'a>(xml: &'a str, tag: &str, from: usize) -> Option<(&'a str, usize)> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml[from..].find(&open)? + from + open.len();
    let end = xml[start..].find(&close)? + start;
    Some((&xml[start..end], end + close.len()))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

impl S3Client {
    pub fn new(target: &S3Target) -> CoreResult<Self> {
        let host = target
            .endpoint
            .split_once("://")
            .map(|(_, host)| host.to_string())
            .ok_or_else(|| CoreError::Validation("S3 endpoint needs a scheme".into()))?;
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(3600))
            .build()
            .map_err(|e| CoreError::External(format!("building S3 client: {e}")))?;
        Ok(Self {
            http,
            target: target.clone(),
            host,
        })
    }

    fn secrets(&self) -> [&str; 2] {
        [&self.target.secret_key, &self.target.access_key]
    }

    fn path(&self, key: &str) -> String {
        if key.is_empty() {
            format!("/{}", self.target.bucket)
        } else {
            format!("/{}/{key}", self.target.bucket)
        }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        key: &str,
        query: &[(&str, &str)],
        body: Vec<u8>,
    ) -> CoreResult<reqwest::Response> {
        let path = self.path(key);
        let payload = if body.is_empty() {
            EMPTY_SHA256.to_string()
        } else {
            sha256_hex(&body)
        };
        let signed = sign(
            method.as_str(),
            &self.host,
            &path,
            query,
            &payload,
            Utc::now(),
            &self.target.region,
            &self.target.access_key,
            &self.target.secret_key,
        );
        let mut url = format!("{}{}", self.target.endpoint, uri_encode(&path, true));
        if !query.is_empty() {
            url.push('?');
            url.push_str(&canonical_query(query));
        }
        let response = self
            .http
            .request(method, &url)
            .header("x-amz-date", signed.amz_date)
            .header("x-amz-content-sha256", signed.content_sha256)
            .header("authorization", signed.authorization)
            .body(body)
            .send()
            .await
            .map_err(|e| {
                CoreError::External(redact(&format!("S3 request failed: {e}"), &self.secrets()))
            })?;
        Ok(response)
    }

    async fn ok_text(&self, response: reqwest::Response, what: &str) -> CoreResult<String> {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(text)
        } else {
            let snippet: String = text.chars().take(500).collect();
            Err(CoreError::External(redact(
                &format!("S3 {what} failed ({status}): {snippet}"),
                &self.secrets(),
            )))
        }
    }

    /// Cheap credential/bucket check: lists at most one key.
    pub async fn check(&self) -> CoreResult<()> {
        let r = self
            .request(
                reqwest::Method::GET,
                "",
                &[("list-type", "2"), ("max-keys", "1")],
                vec![],
            )
            .await?;
        self.ok_text(r, "bucket check").await.map(|_| ())
    }

    pub async fn put_object(&self, key: &str, body: Vec<u8>) -> CoreResult<()> {
        let r = self.request(reqwest::Method::PUT, key, &[], body).await?;
        self.ok_text(r, "upload").await.map(|_| ())
    }

    async fn create_multipart(&self, key: &str) -> CoreResult<String> {
        let r = self
            .request(reqwest::Method::POST, key, &[("uploads", "")], vec![])
            .await?;
        let text = self.ok_text(r, "create multipart upload").await?;
        tag_value(&text, "UploadId", 0)
            .map(|(id, _)| id.to_string())
            .ok_or_else(|| CoreError::External("S3 returned no UploadId".into()))
    }

    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        number: u32,
        body: Vec<u8>,
    ) -> CoreResult<String> {
        let number = number.to_string();
        let r = self
            .request(
                reqwest::Method::PUT,
                key,
                &[("partNumber", &number), ("uploadId", upload_id)],
                body,
            )
            .await?;
        let etag = r
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        self.ok_text(r, "upload part").await?;
        etag.ok_or_else(|| CoreError::External("S3 returned no ETag for a part".into()))
    }

    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        etags: &[String],
    ) -> CoreResult<()> {
        let mut body = String::from("<CompleteMultipartUpload>");
        for (i, etag) in etags.iter().enumerate() {
            body.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                i + 1,
                etag.replace('&', "&amp;").replace('<', "&lt;")
            ));
        }
        body.push_str("</CompleteMultipartUpload>");
        let r = self
            .request(
                reqwest::Method::POST,
                key,
                &[("uploadId", upload_id)],
                body.into_bytes(),
            )
            .await?;
        let text = self.ok_text(r, "complete multipart upload").await?;
        // S3 may answer 200 with an <Error> body.
        if text.contains("<Error>") {
            return Err(CoreError::External(redact(
                &format!("S3 complete multipart upload failed: {text}"),
                &self.secrets(),
            )));
        }
        Ok(())
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) {
        let _ = self
            .request(
                reqwest::Method::DELETE,
                key,
                &[("uploadId", upload_id)],
                vec![],
            )
            .await;
    }

    /// Streams `reader` to `key`. Small inputs (one part or less) use a
    /// single PUT; larger ones a multipart upload that is aborted on any
    /// failure so no orphaned parts keep billing. Returns the bytes sent.
    pub async fn upload_stream<R: AsyncRead + Unpin>(
        &self,
        key: &str,
        reader: &mut R,
    ) -> CoreResult<u64> {
        let first = read_part(reader).await?;
        if first.len() < PART_SIZE {
            let n = first.len() as u64;
            self.put_object(key, first).await?;
            return Ok(n);
        }
        let upload_id = self.create_multipart(key).await?;
        match self.upload_parts(key, &upload_id, first, reader).await {
            Ok(total) => Ok(total),
            Err(e) => {
                self.abort_multipart(key, &upload_id).await;
                Err(e)
            }
        }
    }

    async fn upload_parts<R: AsyncRead + Unpin>(
        &self,
        key: &str,
        upload_id: &str,
        first: Vec<u8>,
        reader: &mut R,
    ) -> CoreResult<u64> {
        let mut etags = Vec::new();
        let mut total = 0u64;
        let mut part = first;
        loop {
            let last = part.len() < PART_SIZE;
            if !part.is_empty() {
                if etags.len() as u32 >= MAX_PARTS {
                    return Err(CoreError::External(
                        "backup exceeds the S3 part limit".into(),
                    ));
                }
                total += part.len() as u64;
                let etag = self
                    .upload_part(key, upload_id, etags.len() as u32 + 1, part)
                    .await?;
                etags.push(etag);
            }
            if last {
                break;
            }
            part = read_part(reader).await?;
        }
        self.complete_multipart(key, upload_id, &etags).await?;
        Ok(total)
    }

    /// All objects whose key starts with `prefix`.
    pub async fn list(&self, prefix: &str) -> CoreResult<Vec<BackupObject>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", prefix)];
            if let Some(t) = &token {
                query.push(("continuation-token", t));
            }
            let r = self
                .request(reqwest::Method::GET, "", &query, vec![])
                .await?;
            let xml = self.ok_text(r, "list").await?;
            let mut at = 0;
            while let Some((entry, next)) = tag_value(&xml, "Contents", at) {
                at = next;
                let key = tag_value(entry, "Key", 0).map(|(k, _)| xml_unescape(k));
                let size = tag_value(entry, "Size", 0).and_then(|(s, _)| s.parse().ok());
                let modified = tag_value(entry, "LastModified", 0).map(|(m, _)| m.to_string());
                if let (Some(key), Some(size_bytes), Some(last_modified)) = (key, size, modified) {
                    out.push(BackupObject {
                        key,
                        size_bytes,
                        last_modified,
                    });
                }
            }
            let truncated = tag_value(&xml, "IsTruncated", 0).is_some_and(|(v, _)| v == "true");
            token = tag_value(&xml, "NextContinuationToken", 0).map(|(t, _)| xml_unescape(t));
            if !truncated || token.is_none() {
                break;
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    pub async fn delete(&self, key: &str) -> CoreResult<()> {
        let r = self
            .request(reqwest::Method::DELETE, key, &[], vec![])
            .await?;
        self.ok_text(r, "delete").await.map(|_| ())
    }

    /// Streams the object into `writer` chunk by chunk.
    pub async fn download_to<W: AsyncWrite + Unpin>(
        &self,
        key: &str,
        writer: &mut W,
    ) -> CoreResult<u64> {
        let mut r = self.request(reqwest::Method::GET, key, &[], vec![]).await?;
        if !r.status().is_success() {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            return Err(CoreError::External(redact(
                &format!(
                    "S3 download failed ({status}): {}",
                    text.chars().take(500).collect::<String>()
                ),
                &self.secrets(),
            )));
        }
        let mut total = 0u64;
        while let Some(chunk) = r.chunk().await.map_err(|e| {
            CoreError::External(redact(&format!("S3 download: {e}"), &self.secrets()))
        })? {
            total += chunk.len() as u64;
            writer
                .write_all(&chunk)
                .await
                .map_err(|e| CoreError::Io(format!("restore pipe closed: {e}")))?;
        }
        writer
            .flush()
            .await
            .map_err(|e| CoreError::Io(format!("restore pipe: {e}")))?;
        Ok(total)
    }
}

/// Fills one buffer of up to [`PART_SIZE`] bytes (fewer only at EOF).
async fn read_part<R: AsyncRead + Unpin>(reader: &mut R) -> CoreResult<Vec<u8>> {
    let mut buf = Vec::with_capacity(PART_SIZE.min(1 << 20));
    let mut limited = reader.take(PART_SIZE as u64);
    limited
        .read_to_end(&mut buf)
        .await
        .map_err(|e| CoreError::Io(format!("reading backup stream: {e}")))?;
    Ok(buf)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    /// Documented AWS example (GET Object with a Range header excluded):
    /// "Example: GET Object" from the SigV4 S3 docs, adapted to our fixed
    /// signed-header set, so this test pins the HMAC chain itself.
    #[test]
    fn signing_chain_matches_known_vector() {
        // AWS docs: derive signing key for 20130524/us-east-1/s3 with the
        // example secret; the signature of a fixed string-to-sign.
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let k_date = hmac(format!("AWS4{secret}").as_bytes(), b"20130524");
        let k_region = hmac(&k_date, b"us-east-1");
        let k_service = hmac(&k_region, b"s3");
        let k_signing = hmac(&k_service, b"aws4_request");
        let sts = "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972";
        assert_eq!(
            hex(&hmac(&k_signing, sts.as_bytes())),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn encodes_paths_and_queries_canonically() {
        assert_eq!(uri_encode("/b/a b/é~.txt", true), "/b/a%20b/%C3%A9~.txt");
        assert_eq!(
            canonical_query(&[("uploads", ""), ("a b", "c/d")]),
            "a%20b=c%2Fd&uploads="
        );
        let now = DateTime::parse_from_rfc3339("2013-05-24T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let s = sign(
            "GET",
            "h:9000",
            "/b/k",
            &[],
            EMPTY_SHA256,
            now,
            "us-east-1",
            "AK",
            "SK",
        );
        assert!(s.authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AK/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="
        ));
        assert!(!s.authorization.contains("SK"));
    }

    #[test]
    fn redacts_configured_secrets_only() {
        assert_eq!(
            redact("bad key s3cr3t-value here", &["s3cr3t-value", "ab"]),
            "bad key ******** here"
        );
    }

    /// Minimal S3 stand-in: stores objects, supports multipart upload,
    /// list-type=2, delete, and GET, and records every Authorization header.
    type Part = (u32, Vec<u8>);

    #[derive(Default)]
    pub struct MockS3 {
        pub objects: Mutex<HashMap<String, Vec<u8>>>,
        uploads: Mutex<HashMap<String, Vec<Part>>>,
        pub auth_headers: Mutex<Vec<String>>,
        pub multipart_completed: Mutex<u32>,
    }

    pub async fn start_mock_s3() -> (String, Arc<MockS3>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(MockS3::default());
        let shared = state.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let state = shared.clone();
                tokio::spawn(async move {
                    let _ = serve(&mut sock, &state).await;
                });
            }
        });
        (endpoint, state)
    }

    async fn serve(sock: &mut tokio::net::TcpStream, st: &MockS3) -> std::io::Result<()> {
        loop {
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if sock.read(&mut byte).await? == 0 {
                    return Ok(());
                }
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head).to_string();
            let mut lines = head.lines();
            let mut first = lines.next().unwrap().split(' ');
            let method = first.next().unwrap().to_string();
            let target = first.next().unwrap().to_string();
            let mut len = 0usize;
            for l in lines {
                let lower = l.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if lower.starts_with("authorization:") {
                    st.auth_headers.lock().unwrap().push(l.to_string());
                }
            }
            let mut body = vec![0u8; len];
            sock.read_exact(&mut body).await?;
            let (path, query) = target.split_once('?').unwrap_or((&target, ""));
            let q: HashMap<String, String> = query
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|p| {
                    let (k, v) = p.split_once('=').unwrap_or((p, ""));
                    (k.to_string(), v.replace("%2F", "/"))
                })
                .collect();
            let key = path
                .trim_start_matches('/')
                .split_once('/')
                .map(|x| x.1)
                .unwrap_or("")
                .to_string();
            let (status, extra, out): (u16, String, Vec<u8>) = match method.as_str() {
                "POST" if q.contains_key("uploads") => {
                    st.uploads.lock().unwrap().insert("u1".into(), vec![]);
                    (200, String::new(), b"<InitiateMultipartUploadResult><UploadId>u1</UploadId></InitiateMultipartUploadResult>".to_vec())
                }
                "PUT" if q.contains_key("partNumber") => {
                    let n: u32 = q["partNumber"].parse().unwrap();
                    st.uploads
                        .lock()
                        .unwrap()
                        .get_mut(&q["uploadId"])
                        .unwrap()
                        .push((n, body));
                    (200, format!("ETag: \"etag{n}\"\r\n"), vec![])
                }
                "POST" if q.contains_key("uploadId") => {
                    let mut parts = st.uploads.lock().unwrap().remove(&q["uploadId"]).unwrap();
                    parts.sort_by_key(|p| p.0);
                    st.objects
                        .lock()
                        .unwrap()
                        .insert(key, parts.into_iter().flat_map(|p| p.1).collect());
                    *st.multipart_completed.lock().unwrap() += 1;
                    (
                        200,
                        String::new(),
                        b"<CompleteMultipartUploadResult/>".to_vec(),
                    )
                }
                "DELETE" if q.contains_key("uploadId") => {
                    st.uploads.lock().unwrap().remove(&q["uploadId"]);
                    (204, String::new(), vec![])
                }
                "PUT" => {
                    st.objects.lock().unwrap().insert(key, body);
                    (200, String::new(), vec![])
                }
                "DELETE" => {
                    st.objects.lock().unwrap().remove(&key);
                    (204, String::new(), vec![])
                }
                "GET" if key.is_empty() => {
                    let prefix = q.get("prefix").cloned().unwrap_or_default();
                    let mut xml =
                        String::from("<ListBucketResult><IsTruncated>false</IsTruncated>");
                    for (k, v) in st
                        .objects
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(k, _)| k.starts_with(&prefix))
                    {
                        xml.push_str(&format!("<Contents><Key>{k}</Key><LastModified>2026-01-01T00:00:00.000Z</LastModified><Size>{}</Size></Contents>", v.len()));
                    }
                    xml.push_str("</ListBucketResult>");
                    (200, String::new(), xml.into_bytes())
                }
                "GET" => match st.objects.lock().unwrap().get(&key) {
                    Some(v) => (200, String::new(), v.clone()),
                    None => (
                        404,
                        String::new(),
                        b"<Error><Code>NoSuchKey</Code></Error>".to_vec(),
                    ),
                },
                _ => (400, String::new(), vec![]),
            };
            let header = format!(
                "HTTP/1.1 {status} X\r\n{extra}Content-Length: {}\r\n\r\n",
                out.len()
            );
            sock.write_all(header.as_bytes()).await?;
            sock.write_all(&out).await?;
        }
    }

    pub fn target(endpoint: &str) -> S3Target {
        S3Target {
            endpoint: endpoint.into(),
            region: "us-east-1".into(),
            bucket: "bk".into(),
            access_key: "AKIATESTKEY".into(),
            secret_key: "topsecretvalue123".into(),
        }
    }

    #[tokio::test]
    async fn streams_small_and_multipart_uploads_and_reads_them_back() {
        let (endpoint, mock) = start_mock_s3().await;
        let client = S3Client::new(&target(&endpoint)).unwrap();
        client.check().await.unwrap();

        // Small: one PUT.
        let mut small: &[u8] = b"hello";
        assert_eq!(
            client.upload_stream("p/small", &mut small).await.unwrap(),
            5
        );
        assert_eq!(*mock.multipart_completed.lock().unwrap(), 0);

        // Large: 2 full parts + a tail, via multipart.
        let big: Vec<u8> = (0..(PART_SIZE * 2 + 1234))
            .map(|i| (i % 251) as u8)
            .collect();
        let mut reader: &[u8] = &big;
        assert_eq!(
            client.upload_stream("p/big", &mut reader).await.unwrap(),
            big.len() as u64
        );
        assert_eq!(*mock.multipart_completed.lock().unwrap(), 1);
        assert_eq!(mock.objects.lock().unwrap()["p/big"], big);

        let mut downloaded = Vec::new();
        client.download_to("p/big", &mut downloaded).await.unwrap();
        assert_eq!(downloaded, big);

        let listed = client.list("p/").await.unwrap();
        assert_eq!(
            listed.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(),
            ["p/big", "p/small"]
        );
        client.delete("p/small").await.unwrap();
        assert_eq!(client.list("p/").await.unwrap().len(), 1);

        // Every request was signed, and no header carries the secret key.
        let headers = mock.auth_headers.lock().unwrap();
        assert!(headers.len() > 5);
        assert!(headers
            .iter()
            .all(|h| h.contains("AWS4-HMAC-SHA256") && !h.contains("topsecretvalue123")));
    }

    #[tokio::test]
    async fn errors_never_contain_the_secret_key() {
        let (endpoint, _mock) = start_mock_s3().await;
        let client = S3Client::new(&target(&endpoint)).unwrap();
        let mut sink = Vec::new();
        let err = client.download_to("missing", &mut sink).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("404"));
        assert!(!text.contains("topsecretvalue123") && !text.contains("AKIATESTKEY"));
        assert!(!format!("{client:?}").contains("topsecretvalue123"));
    }
}
