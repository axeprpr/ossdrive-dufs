# OSSDrive Dufs

基于 [dufs-material-assets](https://github.com/TransparentLC/dufs-material-assets) 的对象存储文件服务。前端使用 Material 风格界面，后端使用阿里云 OSS 签名 URL：文件上传和下载均由浏览器直接连接 OSS，不经过服务器文件流量。

## 工作方式

- 服务器只负责列目录、权限判断、生成短时效签名 URL、删除和移动对象。
- 上传：浏览器请求 `/api/upload-url`，拿到签名后直接 `PUT` 到 OSS。
- 下载：文件请求由后端生成 OSS 签名 URL 并 `302` 跳转，浏览器直接从 OSS 下载。
- AK/SK 仅通过环境变量提供给后端，不会写入前端或返回给浏览器。

## 运行

```bash
docker run -d --name ossdrive-dufs -p 5000:3000 \
  -e OSS_ENDPOINT=https://oss-cn-hangzhou.aliyuncs.com \
  -e OSS_BUCKET=your-bucket \
  -e OSS_ACCESS_KEY_ID=your-ak \
  -e OSS_ACCESS_KEY_SECRET=your-sk \
  -e DUFS_USER=admin \
  -e DUFS_PASSWORD=change-me \
  ossdrive-dufs:latest
```

需要给 OSS 配置浏览器 CORS，允许当前站点的 `GET, HEAD, PUT` 请求，并允许 `ETag` 等必要响应头。设置 `DUFS_USER` 和 `DUFS_PASSWORD` 后，上传签名、删除、创建目录和移动操作要求 Basic Auth；生产环境必须设置并通过 HTTPS 访问，不要开放匿名签名写入。

## GitHub Actions

推送到 `main` 会自动编译 Linux `amd64`/`arm64` 二进制并构建 Docker 镜像（默认不推送镜像）。创建 `v*` 标签会发布二进制 Release。无需本地安装 Rust、Go 或 Node.js。
