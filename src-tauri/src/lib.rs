#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![extract_pdf_tables])
        .run(tauri::generate_context!())
        .expect("failed to run PDF Table Workshop");
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ExtractedTable {
    page: usize,
    table: usize,
    rows: Vec<Vec<String>>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ExtractionResult {
    page_count: usize,
    tables: Vec<ExtractedTable>,
    pages_without_text: Vec<usize>,
    ocr_used: bool,
}

#[tauri::command]
fn extract_pdf_tables(app: tauri::AppHandle, path: String) -> Result<ExtractionResult, String> {
    use tauri::Manager;
    use pdf_oxide::ocr::{OcrConfig, OcrEngine};
    use pdf_oxide::structure::spatial_table_detector::{SpatialTableDetector, TableDetectionConfig};
    use std::io::Read;

    let document = pdf_oxide::PdfDocument::open(&path)
        .map_err(|error| format!("无法打开 PDF：{error}"))?;
    let page_count = document.page_count().map_err(|error| format!("无法读取页数：{error}"))?;
    let mut pages_without_text = Vec::new();
    for page_index in 0..page_count {
        if document.extract_spans(page_index)
            .map_err(|error| format!("第 {} 页读取文字失败：{error}", page_index + 1))?
            .iter().all(|span| span.text.trim().is_empty())
        {
            pages_without_text.push(page_index + 1);
        }
    }

    let mut ocr_engine = None;
    if !pages_without_text.is_empty() {
        let cache = app.path().app_cache_dir().map_err(|e| format!("无法定位 OCR 模型缓存目录：{e}"))?.join("paddleocr-chinese");
        std::fs::create_dir_all(&cache).map_err(|e| format!("无法创建 OCR 模型缓存目录：{e}"))?;
        let spec = pdf_oxide::extractors::auto::OcrLanguage::Chinese.spec();
        let models = [
            ("det.onnx", pdf_oxide::extractors::auto::OcrLanguage::DET_URL),
            (spec.rec_file.as_str(), spec.rec_url.as_str()),
            (spec.dict_file.as_str(), spec.dict_url.as_str()),
        ];
        // 模型已随安装包内置：优先从资源目录复制到缓存，仅在缺失时联网下载兜底
        let bundled = app.path().resource_dir().ok().map(|dir| dir.join("ocr-models"));
        let dev_bundled = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/ocr-models");
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(180)))
            .build().new_agent();
        for (filename, url) in models {
            let destination = cache.join(filename);
            if destination.is_file() { continue; }
            let source = bundled.as_ref().map(|dir| dir.join(filename)).filter(|path| path.is_file())
                .or_else(|| { let path = dev_bundled.join(filename); path.is_file().then_some(path) });
            if let Some(source) = source {
                std::fs::copy(&source, &destination)
                    .map_err(|e| format!("安装内置 OCR 模型失败（{filename}）：{e}"))?;
                continue;
            }
            let mut response = agent.get(url).call()
                .map_err(|e| format!("下载中文 OCR 模型失败（{filename}）：{e}"))?;
            let mut bytes = Vec::new();
            response.body_mut().as_reader().read_to_end(&mut bytes)
                .map_err(|e| format!("读取中文 OCR 模型失败（{filename}）：{e}"))?;
            if bytes.len() < 256 { return Err(format!("下载的 OCR 文件无效：{filename}")); }
            let temporary = destination.with_extension("part");
            std::fs::write(&temporary, bytes).map_err(|e| format!("保存 OCR 模型失败：{e}"))?;
            std::fs::rename(temporary, destination).map_err(|e| format!("安装 OCR 模型失败：{e}"))?;
        }
        let det = cache.join("det.onnx");
        let rec = cache.join(spec.rec_file);
        let dict = cache.join(spec.dict_file);
        ocr_engine = Some(OcrEngine::new(det, rec, dict, OcrConfig::default())
            .map_err(|e| format!("加载中文 OCR 模型失败：{e}"))?);
    }

    let mut tables = Vec::new();
    let detector = SpatialTableDetector::with_config(TableDetectionConfig::relaxed());
    let mut ocr_used = false;
    for page_index in 0..page_count {
        let mut page_tables = document.extract_tables(page_index)
            .map_err(|error| format!("第 {} 页识别失败：{error}", page_index + 1))?;
        if page_tables.is_empty() && pages_without_text.contains(&(page_index + 1)) {
            if let Some(engine) = ocr_engine.as_ref() {
                let images = document.extract_images(page_index)
                    .map_err(|e| format!("第 {} 页读取扫描图片失败：{e}", page_index + 1))?;
                let mut spans = Vec::new();
                for image in images {
                    let Some(bbox) = image.bbox() else { continue };
                    if image.width() == 0 || image.height() == 0 || bbox.width <= 0.0 || bbox.height <= 0.0 { continue; }
                    let raster = image.to_dynamic_image().map_err(|e| format!("第 {} 页解码扫描图片失败：{e}", page_index + 1))?;
                    let recognized = engine.ocr_image(&raster).map_err(|e| format!("第 {} 页 OCR 识别失败：{e}", page_index + 1))?;
                    let scale_x = bbox.width / image.width() as f32;
                    let scale_y = bbox.height / image.height() as f32;
                    for mut span in recognized.to_text_spans(1.0) {
                        span.bbox.x = bbox.x + span.bbox.x * scale_x;
                        span.bbox.y = bbox.y + span.bbox.y * scale_y;
                        span.bbox.width *= scale_x;
                        span.bbox.height *= scale_y;
                        span.font_size *= (scale_x + scale_y) / 2.0;
                        spans.push(span);
                    }
                }
                if !spans.is_empty() {
                    ocr_used = true;
                    page_tables = detector.detect_tables_hybrid(&spans, &[]);
                }
            }
        }
        for (table_index, table) in page_tables.into_iter().enumerate() {
            let width = table.col_count.max(table.rows.iter().map(|row| row.cells.len()).max().unwrap_or(0));
            let rows = table.rows.into_iter().map(|row| {
                let mut cells = row.cells.into_iter().map(|cell| cell.text.trim().to_string()).collect::<Vec<_>>();
                cells.resize(width, String::new());
                cells
            }).collect();
            tables.push(ExtractedTable { page: page_index + 1, table: table_index + 1, rows });
        }
    }
    Ok(ExtractionResult { page_count, tables, pages_without_text, ocr_used })
}
