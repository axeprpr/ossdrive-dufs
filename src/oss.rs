use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use hmac::{Hmac, Mac};
use hyper::{body::Incoming, header, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::Client;
use sha1::Sha1;
use std::{collections::BTreeMap, sync::Arc};

use crate::server::Response;

type HmacSha1 = Hmac<Sha1>;

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
        let bucket_endpoint = format!("{}/{}", endpoint.trim_end_matches('/'), bucket);
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
            return Ok(redirect_response(self.signed_url(path, "GET")?));
        }
        Ok(error_response(StatusCode::METHOD_NOT_ALLOWED, "OSS mode requires /api/upload-url for uploads"))
    }

    fn authorized(&self, req: &Request<Incoming>) -> bool { self.user.as_ref().map(|(u,p)| req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).is_some_and(|v| v == format!("Basic {}", STANDARD.encode(format!("{}:{}",u,p)) ))).unwrap_or(true) }

    async fn upload_url(&self, req: Request<Incoming>) -> Result<Response> {
        let body = req.into_body().collect().await?.to_bytes();
        let input: serde_json::Value = serde_json::from_slice(&body).context("invalid JSON")?;
        let name = input.get("name").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("name is required"))?;
        let key = self.key(name)?;
        Ok(json_response(StatusCode::OK, serde_json::to_vec(&serde_json::json!({"url": self.signed_url(&key, "PUT")?}))?.as_slice()))
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
    fn signed_api_url(&self, method: &str, key: &str, query: &BTreeMap<&str,String>) -> Result<String> { let resource_base=format!("/{}",key); let expires=(Utc::now().timestamp()+900).to_string(); let canonical_query=query.iter().map(|(k,v)|format!("{}={}",k,utf8_percent_encode(v,NON_ALPHANUMERIC))).collect::<Vec<_>>().join("&"); let canonical_resource=if canonical_query.is_empty(){resource_base.clone()}else{format!("{}?{}",resource_base,canonical_query)}; let mut q=query.clone(); q.insert("OSSAccessKeyId",self.access_key.clone()); q.insert("Expires",expires); let query_string=q.iter().map(|(k,v)|format!("{}={}",k,utf8_percent_encode(v,NON_ALPHANUMERIC))).collect::<Vec<_>>().join("&"); let string_to_sign=format!("{}\n\n\n{}\n{}",method,q.get("Expires").unwrap(),canonical_resource); let mut mac=HmacSha1::new_from_slice(self.secret.as_bytes()).map_err(|_|anyhow!("invalid secret"))?; mac.update(string_to_sign.as_bytes()); let signature=STANDARD.encode(mac.finalize().into_bytes()); Ok(format!("{}{}?{}&Signature={}",self.bucket_endpoint,resource_base,query_string,utf8_percent_encode(&signature,NON_ALPHANUMERIC))) }
}

fn first_env(keys: &[&str]) -> Result<String> { keys.iter().find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty())).ok_or_else(|| anyhow!("missing {}", keys.join(" or "))) }
fn response(status: StatusCode, body: Bytes) -> Response { let mut response=Response::new(Full::new(body).map_err(|e| anyhow!(e)).boxed()); *response.status_mut()=status; response }
fn json_response(status: StatusCode, body: &[u8]) -> Response { let mut r=response(status,Bytes::copy_from_slice(body)); r.headers_mut().insert(header::CONTENT_TYPE,header::HeaderValue::from_static("application/json")); r }
fn error_response(status: StatusCode, message: &str) -> Response { json_response(status,serde_json::json!({"error":message}).to_string().as_bytes()) }
fn unauthorized() -> Response { let mut r=error_response(StatusCode::UNAUTHORIZED,"authentication required"); r.headers_mut().insert(header::WWW_AUTHENTICATE,header::HeaderValue::from_static("Basic realm=ossdrive")); r }
fn redirect_response(url: String) -> Response { let mut r=response(StatusCode::FOUND,Bytes::new()); r.headers_mut().insert(header::LOCATION,header::HeaderValue::try_from(url).unwrap()); r }
