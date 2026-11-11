# PDF 表格工坊

Vue 3 + Tauri 2 桌面应用。参考 `../todolist` 的 Tauri 2 架构，支持 macOS Intel（`x86_64-apple-darwin`）和 Windows x64（NSIS）。

## 开发

```sh
npm install
npm run tauri dev
```

## 打包

GitHub Actions 会在推送 `v*` Tag 后自动生成 macOS Intel DMG 和 Windows x64 NSIS 安装包，并发布 GitHub Release。Intel Mac 使用 `macos-15-intel` runner，避免自动选择 ARM runner。`package.json` 的 `version` 是唯一版本源；运行 `npm run sync-version` 会同步更新 npm 锁文件、Tauri 配置和 Cargo 清单。发布前修改 `package.json` 版本，运行同步命令并提交，然后创建同版本 Tag。workflow 会自动同步并检查 Tag，也会确认 updater Secret 已设置。

```sh
# 修改 package.json 的 version 为 0.1.2 后：
npm run sync-version
git add package.json package-lock.json scripts/sync-version.mjs src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "chore: prepare release v0.1.2"
git tag v0.1.2
git push origin main
git push origin v0.1.2
```

也可以在 GitHub 仓库的 Actions 页面手动运行 `Release PDF Table Workshop`，输入已提交的版本 Tag。Windows 构建在 Windows runner 上执行，Intel Mac 构建在 Intel runner 上执行。本 workflow 不构建 Android 包。

### 自动更新签名

Tauri updater 密钥对已生成：公钥已写入 `src-tauri/tauri.conf.json`；无密码私钥保存在仓库外的 `/Users/xbao/.tauri/pdf2excel-updater`，权限为仅当前用户可读写。打包时使用：

```sh
TAURI_SIGNING_PRIVATE_KEY=/Users/xbao/.tauri/pdf2excel-updater npm run tauri build
```

发布时 workflow 会在 `keerkeerhi/pdf2excel` 的 GitHub Releases 上传签名产物及 `latest.json`。私钥丢失后，已发布版本无法再用新的密钥签名更新。

自动发布 workflow 需要仓库 Secret `TAURI_SIGNING_PRIVATE_KEY`。在本机项目目录运行下面的命令，将本机私钥内容安全地传给 GitHub CLI（命令不会把私钥打印到终端）：

```sh
gh secret set TAURI_SIGNING_PRIVATE_KEY < /Users/xbao/.tauri/pdf2excel-updater
```

仓库 remote 已对应 `keerkeerhi/pdf2excel`，Updater endpoint 已指向该仓库的 `latest.json`。配置好 Secret 后，Tag 发布会同时上传签名安装包和 `latest.json`。

这把密钥只用于 Tauri 自动更新签名，不是 Apple Developer ID 或 Windows Authenticode 代码签名证书。本机当前没有可用的 macOS 代码签名身份；macOS 公证和 Windows 发布者签名需要另行取得对应证书。

## PDF 处理说明

PDF.js 只负责页面预览和渲染。`pdf_oxide` 通过 Tauri Rust command 执行页面级表格检测；扫描页使用本地 PaddleOCR ONNX 中文模型识别文字坐标，再由 `pdf_oxide` 重建表格。模型已内置在安装包中（`src-tauri/resources/ocr-models/`，约 89MB），首次使用时复制到应用 cache 目录，无需联网下载；仅在内置文件缺失时才会回退到在线下载。识别过程在本机运行，不上传 PDF。结果会显示在可编辑表格中，并可批量导出为每个 PDF 独立的 Excel 工作表。其他识别器尚未接入。
