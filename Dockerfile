FROM node:22-alpine AS frontend
WORKDIR /src
COPY package.json package-lock.json pnpm-workspace.yaml vite.config.js after-build.js index.html ./
COPY src ./src
COPY public ./public
RUN npm ci --no-audit --no-fund && npm run build

FROM golang:1.24-alpine AS backend
WORKDIR /src
ARG GOPROXY=https://proxy.golang.org,direct
ENV GOPROXY=$GOPROXY
COPY backend/go.mod ./
RUN go mod download
COPY backend/main.go ./
COPY --from=frontend /src/dist ./web/dist
RUN CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' -o /ossdrive .

FROM alpine:3.21
RUN adduser -D -H -u 10001 app
COPY --from=backend /ossdrive /ossdrive
USER app
EXPOSE 3000
ENTRYPOINT ["/ossdrive"]
