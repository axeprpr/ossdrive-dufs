use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{Datelike, Timelike, Utc};
use hmac::{Hmac, Mac};
use hyper::{body::Incoming, header, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS, NON_ALPHANUMERIC};
use reqwest::Client;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc};

use crate::server::Response;

type HmacSha1 = Hmac<Sha1>;
const OSS_ENCODE_SET: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'<').add(b'>').add(b'`').add(b'#').add(b'?').add(b'/').add(b':').add(b'=').add(b'&').add(b'+');

pub struct OssServer {
    client: Client,
    endpoint: String,
    bucket: String,
    bucket_endpoint: String,
    access_key: String,
    secret: String,
    prefix: String,
    user: Option<(String, String)>,
}

impl OssServer {
    pub fn from_env() -> Result<Self> {
        let endpoint = std::env::var("DUFS_OSS_ENDPOINT").or_else(|_| std::env::var("OSS_ENDPOINT")).unwrap_or_else(|_| "https://oss-cn-hangzhou.aliyuncs.com".into());
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let bucket = first_env(&["DUFS_OSS_BUCKET", "OSS_BUCKET"])?;
        let bucket_endpoint = endpoint.replacen("://", &format!("://{}.", bucket), 1);
        Ok(Self { client: Client::new(), endpoint, bucket_endpoint, bucket, access_key: first_env(&["DUFS_OSS_ACCESS_KEY_ID", "OSS_ACCESS_KEY_ID"] )?, secret: first_env(&["DUFS_OSS_ACCESS_KEY_SECRET", "OSS_ACCESS_KEY_SECRET"] )?, prefix: first_env(&["DUFS_OSS_PREFIX", "OSS_PREFIX"]).unwrap_or_default().trim_matches('/').to_string(), user: match (std::env::var("DUFS_USER"), std::env::var("DUFS_PASSWORD")) { (Ok(u), Ok(p)) if !u.is_empty() => Some((u,p)), _ => None } })
    }

    pub async fn call(self: Arc<Self>, req: Request<Incoming>, _addr: Option<std::net::SocketAddr>) -> Result<Response, hyper::Error> {
        match self.handle(req).await { Ok(r) => Ok(r), Err(e) => Ok(error_response(StatusCode::BAD_GATEWAY, &e.to_string())) }
    }

    async fn handle(&self, req: Request<Incoming>) -> Result<Response> {
        if req.method() == Method::OPTIONS { return Ok(response(StatusCode::NO_CONTENT, Bytes::new())); }
        let path = req.uri().path().trim_matches('/');
        if path == "health" || path == "__dufs__/health" { return Ok(json_response(StatusCode::OK, br#"{"status":"ok"}"#)); }
        if req.method().as_str() == "CHECKAUTH" { return Ok(if self.authorized(&req) { response(StatusCode::OK, Bytes::new()) } else { unauthorized() }); }
        if path == "api/upload-url" { if !self.authorized(&req) { return Ok(unauthorized()); }; return self.upload_url(req).await; }
        if req.method() == Method::DELETE { if !self.authorized(&req) { return Ok(unauthorized()); }; return self.delete(path).await; }
        if req.method().as_str() == "MOVE" { if !self.authorized(&req) { return Ok(unauthorized()); }; return self.move_object(path, req.headers().get("destination").and_then(|v| v.to_str().ok())).await; }
        if req.method() == Method::GET || req.method() == Method::HEAD {
            if req.uri().query().is_some_and(|q| q.split('&').any(|v| v == "json")) { return self.list(path).await; }
            if path.is_empty() || path.ends_with('/') { return self.list(path).await; }
            return Ok(redirect_response(self.signed_url(&self.key(path)?, "GET")?));
        }
        Ok(error_response(StatusCode::METHOD_NOT_ALLOWED, "OSS mode requires /api/upload-url for uploads"))
    }

    fn authorized(&self, req: &Request<Incoming>) -> bool { self.user.as_ref().map(|(u,p)| req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).is_some_and(|v| v == format!("Basic {}", STANDARD.encode(format!("{}:{}",u,p)) ))).unwrap_or(true) }

    async fn upload_url(&self, req: Request<Incoming>) -> Result<Response> {
        let body = req.into_body().collect().await?.to_bytes();
        let input: serde_json::Value = serde_json::from_slice(&body).context("invalid JSON")?;
        let name = input.get("name").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("name is required"))?;
        let key = self.key(name)?;
        Ok(json_response(StatusCode::OK, serde_json::to_vec(&serde_json::json!({"url": self.signed_url(&key, "PUT")?, "headers": {"Content-Type": "application/octet-stream"}}))?.as_slice()))
    }

    async fn list(&self, path: &str) -> Result<Response> {
        let prefix = self.key_prefix(path);
        let mut query = BTreeMap::new(); query.insert("delimiter", "/".to_string()); query.insert("max-keys", "1000".to_string()); query.insert("prefix", prefix.clone());
        let url = self.signed_api_url("GET", "", &query)?;
        let text = self.client.get(url).send().await?.error_for_status()?.text().await?;
        let mut paths = Vec::new();
        for part in text.split("<CommonPrefixes>").skip(1) { if let Some(v) = part.split("<Prefix>").nth(1).and_then(|v| v.split("</Prefix>").next()) { let n = v.trim_start_matches(&prefix).trim_end_matches('/'); if !n.is_empty() { paths.push(serde_json::json!({"path_type":"Dir","name":n})); } } }
        for part in text.split("<Contents>").skip(1) { if let Some(v) = part.split("<Key>").nth(1).and_then(|v| v.split("</Key>").next()) { let n=v.trim_start_matches(&prefix); if !n.is_empty() && !n.contains('/') { let size=part.split("<Size>").nth(1).and_then(|x|x.split("</Size>").next()).unwrap_or("0"); paths.push(serde_json::json!({"path_type":"File","name":n,"size":size.parse::<u64>().unwrap_or(0)})); } } }
        Ok(json_response(StatusCode::OK, serde_json::to_vec(&serde_json::json!({"paths":paths,"allow_upload":true,"allow_delete":true,"allow_search":true,"allow_archive":false,"auth":self.user.is_some()}))?.as_slice()))
    }

    async fn delete(&self, path: &str) -> Result<Response> { let key=self.key(path)?; let url=self.signed_api_url("DELETE", &key, &BTreeMap::new())?; self.client.delete(url).send().await?.error_for_status()?; Ok(response(StatusCode::NO_CONTENT, Bytes::new())) }
    async fn move_object(&self, path: &str, destination: Option<&str>) -> Result<Response> { let src=self.key(path)?; let dst=self.key(destination.ok_or_else(||anyhow!("Destination is required"))?)?; let url=self.signed_api_url("PUT", &dst, &BTreeMap::new())?; self.client.put(url).header("x-oss-copy-source", format!("/{}/{}",self.bucket,src)).send().await?.error_for_status()?; let _=self.delete(path).await?; Ok(response(StatusCode::CREATED, Bytes::new())) }

    fn key(&self, value: &str) -> Result<String> { let value=urlencoding::decode(value)?.replace('\\', "/"); let clean=value.trim_matches('/'); if clean.is_empty() || clean.split('/').any(|p|p==".."||p.is_empty()) { return Err(anyhow!("invalid path")); } Ok(if self.prefix.is_empty(){clean.to_string()}else{format!("{}/{}",self.prefix,clean)}) }
    fn key_prefix(&self, path: &str) -> String { let raw=path.trim_matches('/'); if self.prefix.is_empty(){ if raw.is_empty(){String::new()}else{format!("{}/",raw)} }else if raw.is_empty(){format!("{}/",self.prefix)}else{format!("{}/{}/",self.prefix,raw)} }
    fn signed_url(&self, key: &str, method: &str) -> Result<String> { Ok(self.signed_api_url(method,key,&BTreeMap::new())?) }
    fn signed_api_url(&self, method: &str, key: &str, query: &BTreeMap<&str,String>) -> Result<String> {
        let now = Utc::now();
        let date = format!("{:04}{:02}{:02}", now.year(), now.month(), now.day());
        let timestamp = format!("{}T{:02}{:02}{:02}Z", date, now.hour(), now.minute(), now.second());
        let region = std::env::var("OSS_REGION").unwrap_or_else(|_| "cn-hangzhou".into());
        let credential = format!("{}/{}/{}/oss/aliyun_v4_request", self.access_key, date, region);
        let path = if key.is_empty() { "/".to_string() } else { format!("/{}", key.split('/').map(|part| utf8_percent_encode(part, NON_ALPHANUMERIC).to_string()).collect::<Vec<_>>().join("/") ) };
        let canonical_path = if key.is_empty() { format!("/{}/", self.bucket) } else { format!("/{}/{}", self.bucket, key.split('/').map(|part| utf8_percent_encode(part, NON_ALPHANUMERIC).to_string()).collect::<Vec<_>>().join("/") ) };
        let mut q = query.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect::<BTreeMap<_, _>>();
        q.insert("x-oss-credential".into(), credential.clone());
        q.insert("x-oss-date".into(), timestamp.clone());
        q.insert("x-oss-expires".into(), "900".into());
        q.insert("x-oss-signature-version".into(), "OSS4-HMAC-SHA256".into());
        q.insert("x-oss-additional-headers".into(), "host".into());
        let query_string = q.iter().map(|(k, v)| format!("{}={}", utf8_percent_encode(k, OSS_ENCODE_SET), utf8_percent_encode(v, OSS_ENCODE_SET))).collect::<Vec<_>>().join("&");
        let host = self.bucket_endpoint.trim_start_matches("https://").trim_start_matches("http://");
        let signed_headers = "host";
        let canonical_headers = if method == "PUT" { format!("content-type:application/octet-stream\nhost:{}\n", host) } else { format!("host:{}\n", host) };
        let payload_hash = "UNSIGNED-PAYLOAD";
        let canonical_request = format!("{}\n{}\n{}\n{}\n{}\n{}", method, canonical_path, query_string, canonical_headers, signed_headers, payload_hash);
        let hashed_request = hex::encode(Sha256::digest(canonical_request.as_bytes()));
        let scope = format!("{}/{}/oss/aliyun_v4_request", date, region);
        let string_to_sign = format!("OSS4-HMAC-SHA256\n{}\n{}\n{}", timestamp, scope, hashed_request);
        let date_key = hmac_sha256(format!("aliyun_v4{}", self.secret).as_bytes(), date.as_bytes());
        let region_key = hmac_sha256(&date_key, region.as_bytes());
        let service_key = hmac_sha256(&region_key, b"oss");
        let signing_key = hmac_sha256(&service_key, b"aliyun_v4_request");
        let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));
        Ok(format!("{}{}?{}&x-oss-signature={}", self.bucket_endpoint, path, query_string, signature))
    }
}

fn first_env(keys: &[&str]) -> Result<String> { keys.iter().find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty())).ok_or_else(|| anyhow!("missing {}", keys.join(" or "))) }
fn hmac_sha256(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut block = [0u8; 64];
    if key.len() > 64 { block[..32].copy_from_slice(&Sha256::digest(key)); } else { block[..key.len()].copy_from_slice(key); }
    let mut inner = [0u8; 64]; let mut outer = [0u8; 64];
    for i in 0..64 { inner[i] = block[i] ^ 0x36; outer[i] = block[i] ^ 0x5c; }
    let mut input = inner.to_vec(); input.extend_from_slice(value);
    let inner_hash = Sha256::digest(&input);
    let mut output = outer.to_vec(); output.extend_from_slice(&inner_hash);
    Sha256::digest(&output).to_vec()
}
fn response(status: StatusCode, body: Bytes) -> Response { let mut response=Response::new(Full::new(body).map_err(|e| anyhow!(e)).boxed()); *response.status_mut()=status; response }
fn json_response(status: StatusCode, body: &[u8]) -> Response { let mut r=response(status,Bytes::copy_from_slice(body)); r.headers_mut().insert(header::CONTENT_TYPE,header::HeaderValue::from_static("application/json")); r }
fn error_response(status: StatusCode, message: &str) -> Response { json_response(status,serde_json::json!({"error":message}).to_string().as_bytes()) }
fn unauthorized() -> Response { let mut r=error_response(StatusCode::UNAUTHORIZED,"authentication required"); r.headers_mut().insert(header::WWW_AUTHENTICATE,header::HeaderValue::from_static("Basic realm=ossdrive")); r }
fn redirect_response(url: String) -> Response { let mut r=response(StatusCode::FOUND,Bytes::new()); r.headers_mut().insert(header::LOCATION,header::HeaderValue::try_from(url).unwrap()); r }
