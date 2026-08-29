package main

import (
	"embed"
	"encoding/json"
	"fmt"
	"io/fs"
	"log"
	"net/http"
	"net/url"
	"os"
	"path"
	"strings"
	"time"

	"github.com/aliyun/aliyun-oss-go-sdk/oss"
)

//go:embed web/dist
var webFiles embed.FS

type app struct { bucket *oss.Bucket; prefix string }
type item struct { Name string `json:"name"`; PathType string `json:"path_type"`; Size int64 `json:"size,omitempty"`; Modified string `json:"modified,omitempty"` }
type listing struct { Paths []item `json:"paths"`; AllowUpload bool `json:"allow_upload"`; AllowDelete bool `json:"allow_delete"`; AllowSearch bool `json:"allow_search"`; AllowArchive bool `json:"allow_archive"`; Auth bool `json:"auth"` }

func main() {
	endpoint := getenv("OSS_ENDPOINT", "https://oss-cn-hangzhou.aliyuncs.com")
	client, err := oss.New(endpoint, os.Getenv("OSS_ACCESS_KEY_ID"), os.Getenv("OSS_ACCESS_KEY_SECRET")); if err != nil { log.Fatal(err) }
	bucket, err := client.Bucket(os.Getenv("OSS_BUCKET")); if err != nil { log.Fatal(err) }
	a := &app{bucket: bucket, prefix: strings.Trim(os.Getenv("OSS_PREFIX"), "/")}
	log.Fatal(http.ListenAndServe(":"+getenv("PORT", "3000"), a.routes()))
}

func getenv(k, fallback string) string { if v := os.Getenv(k); v != "" { return v }; return fallback }
func jsonOut(w http.ResponseWriter, status int, v any) { w.Header().Set("Content-Type", "application/json"); w.Header().Set("Access-Control-Allow-Origin", "*"); w.WriteHeader(status); _ = json.NewEncoder(w).Encode(v) }
func (a *app) key(raw string) (string, error) { n, err := url.PathUnescape(raw); if err != nil { return "", err }; n = strings.TrimLeft(strings.ReplaceAll(n, "\\", "/"), "/"); clean := path.Clean(n); if clean == "." || clean == ".." || strings.HasPrefix(clean, "../") || strings.Contains(clean, "\x00") { return "", fmt.Errorf("invalid path") }; if a.prefix != "" { return a.prefix + "/" + clean, nil }; return clean, nil }
func (a *app) relative(k string) string { return strings.TrimPrefix(strings.TrimPrefix(k, a.prefix), "/") }

func (a *app) routes() http.Handler { mux := http.NewServeMux(); mux.HandleFunc("/api/upload-url", a.uploadURL); mux.HandleFunc("/api/health", health); mux.HandleFunc("/health", health); mux.HandleFunc("/", a.web); return mux }
func (a *app) web(w http.ResponseWriter, r *http.Request) { if r.URL.Path == "/" || strings.HasPrefix(r.URL.Path, "/assets/") || r.URL.Path == "/favicon.ico" { static, err := fs.Sub(webFiles, "web/dist"); if err != nil { http.Error(w, "web unavailable", 500); return }; http.FileServer(http.FS(static)).ServeHTTP(w, r); return }; a.path(w, r) }
func health(w http.ResponseWriter, r *http.Request) { jsonOut(w, http.StatusOK, map[string]string{"status":"ok"}) }

func (a *app) uploadURL(w http.ResponseWriter, r *http.Request) { if !a.authorized(w, r) { return }; if r.Method != http.MethodPost { jsonOut(w, 405, map[string]string{"error":"method not allowed"}); return }; var in struct{ Name string `json:"name"` }; if json.NewDecoder(r.Body).Decode(&in) != nil { jsonOut(w, 400, map[string]string{"error":"invalid request"}); return }; k, err := a.key(in.Name); if err != nil { jsonOut(w, 400, map[string]string{"error":err.Error()}); return }; signed, err := a.bucket.SignURL(k, oss.HTTPPut, 900); if err != nil { jsonOut(w, 502, map[string]string{"error":err.Error()}); return }; jsonOut(w, 200, map[string]string{"url":signed}) }
func (a *app) authorized(w http.ResponseWriter, r *http.Request) bool { user, pass := os.Getenv("DUFS_USER"), os.Getenv("DUFS_PASSWORD"); if user == "" && pass == "" { return true }; gotUser, gotPass, ok := r.BasicAuth(); if !ok || gotUser != user || gotPass != pass { w.Header().Set("WWW-Authenticate", `Basic realm="ossdrive"`); http.Error(w, "authentication required", http.StatusUnauthorized); return false }; return true }
func (a *app) path(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path == "/" && r.URL.Query().Has("json") || strings.HasSuffix(r.URL.Path, "/") && r.URL.Query().Has("json") { a.list(w, r); return }
	if r.Method == http.MethodDelete { if a.authorized(w,r) { a.delete(w, r) }; return }
	if r.Method == "MKCOL" { if a.authorized(w,r) { a.mkdir(w, r) }; return }
	if r.Method == "MOVE" { if a.authorized(w,r) { a.move(w, r) }; return }
	if r.Method != http.MethodGet && r.Method != http.MethodHead { http.Error(w, "direct upload is disabled; request /api/upload-url", 405); return }
	k, err := a.key(strings.TrimPrefix(r.URL.Path, "/")); if err != nil { http.NotFound(w, r); return }; signed, err := a.bucket.SignURL(k, oss.HTTPGet, 900); if err != nil { http.Error(w, "sign download failed", 502); return }; http.Redirect(w, r, signed, http.StatusFound)
}
func (a *app) list(w http.ResponseWriter, r *http.Request) { raw := strings.Trim(r.URL.Path, "/"); prefix := a.prefix; if raw != "" { if prefix != "" { prefix += "/" }; prefix += raw }; if prefix != "" { prefix += "/" }; result := listing{Paths: []item{}, AllowUpload:true, AllowDelete:true, AllowSearch:true, Auth:false}; seen := map[string]bool{}; marker := ""; for { x, err := a.bucket.ListObjects(oss.Prefix(prefix), oss.Marker(marker), oss.MaxKeys(1000)); if err != nil { jsonOut(w, 502, map[string]string{"error":err.Error()}); return }; for _, o := range x.Objects { name := strings.TrimPrefix(a.relative(o.Key), raw+"/"); if name == ".ossdrive-folder" || strings.HasSuffix(name, "/.ossdrive-folder") { continue }; if strings.Contains(name, "/") { name = strings.Split(name, "/")[0]; if !seen[name] { result.Paths = append(result.Paths, item{Name:name, PathType:"Dir"}); seen[name] = true }; continue }; if name != "" && !seen[name] { result.Paths = append(result.Paths, item{Name:name, PathType:"File", Size:int64(o.Size), Modified:o.LastModified.Format(time.RFC3339)}); seen[name] = true } }; if !x.IsTruncated { break }; marker=x.NextMarker }; jsonOut(w, 200, result) }
func (a *app) delete(w http.ResponseWriter, r *http.Request) { k, err := a.key(strings.TrimPrefix(r.URL.Path,"/")); if err != nil { http.NotFound(w,r); return }; x, err := a.bucket.ListObjects(oss.Prefix(strings.TrimSuffix(k,"/")+"/"), oss.MaxKeys(1000)); if err != nil { jsonOut(w,502,map[string]string{"error":err.Error()}); return }; keys := []string{k}; for _, o := range x.Objects { keys=append(keys,o.Key) }; if err=a.bucket.DeleteObjects(keys); err != nil { jsonOut(w,502,map[string]string{"error":err.Error()}); return }; w.WriteHeader(http.StatusNoContent) }
func (a *app) mkdir(w http.ResponseWriter, r *http.Request) { k, err := a.key(strings.TrimSuffix(strings.TrimPrefix(r.URL.Path,"/"),"/")); if err != nil { http.Error(w,"invalid path",400); return }; if err=a.bucket.PutObject(k+"/.ossdrive-folder", strings.NewReader("")); err != nil { http.Error(w,err.Error(),502); return }; w.WriteHeader(http.StatusCreated) }
func (a *app) move(w http.ResponseWriter, r *http.Request) { src,err:=a.key(strings.TrimPrefix(r.URL.Path,"/")); dstRaw:=r.Header.Get("Destination"); if err!=nil||dstRaw=="" { http.Error(w,"invalid destination",400); return }; dst,err:=a.key(strings.TrimPrefix(dstRaw,"/")); if err!=nil { http.Error(w,"invalid destination",400); return }; if err=a.bucket.CopyObject(src,dst); err==nil { err=a.bucket.DeleteObject(src) }; if err!=nil { http.Error(w,err.Error(),502); return }; w.WriteHeader(http.StatusCreated) }
