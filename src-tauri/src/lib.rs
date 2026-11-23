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

/// 构建优化后的 OCR 配置，提高识别精度
fn build_ocr_config() -> pdf_oxide::ocr::OcrConfig {
    let mut config = pdf_oxide::ocr::OcrConfig::default();
    // 更敏感的文本检测 — 扫描件中弱对比度的文字也能被捕捉
    config.det_threshold = 0.25;
    // 过滤低置信度的检测框 — 减少背景噪点被误识别为文字
    config.box_threshold = 0.55;
    // 提高识别置信度阈值 — 只有高置信度的结果才被保留
    config.rec_threshold = 0.6;
    config
}

#[tauri::command]
fn extract_pdf_tables(app: tauri::AppHandle, path: String) -> Result<ExtractionResult, String> {
    use tauri::Manager;
    use pdf_oxide::ocr::OcrEngine;
    use pdf_oxide::rendering::{render_page, RenderOptions, ImageFormat};
    use pdf_oxide::structure::spatial_table_detector::{SpatialTableDetector, TableDetectionConfig};
    use std::io::Read;

    let document = pdf_oxide::PdfDocument::open(&path)
        .map_err(|error| format!("无法打开 PDF：{error}"))?;
    let page_count = document.page_count().map_err(|error| format!("无法读取页数：{error}"))?;

    // 检测哪些页面没有原生文字（扫描件）
    let mut pages_without_text = Vec::new();
    for page_index in 0..page_count {
        if document.extract_spans(page_index)
            .map_err(|error| format!("第 {} 页读取文字失败：{error}", page_index + 1))?
            .iter().all(|span| span.text.trim().is_empty())
        {
            pages_without_text.push(page_index + 1);
        }
    }

    // === 1. 原生有文字的页面：直接提取表格 ===
    let mut tables = Vec::new();
    let mut ocr_used = false;

    // 先收集有原生文字的页面的表格
    let mut ocr_engine = None;
    let mut render_options: Option<RenderOptions> = None;
    let mut page_width_pt = 612.0_f32;  // Letter 兜底
    let mut page_height_pt = 792.0_f32;

    for page_index in 0..page_count {
        let is_scanned = pages_without_text.contains(&(page_index + 1));

        // 非扫描页：直接原生提取
        if !is_scanned {
            let page_tables = document.extract_tables(page_index)
                .map_err(|error| format!("第 {} 页识别失败：{error}", page_index + 1))?;
            for (table_index, table) in page_tables.into_iter().enumerate() {
                let width = table.col_count.max(table.rows.iter().map(|row| row.cells.len()).max().unwrap_or(0));
                let rows = table.rows.into_iter().map(|row| {
                    let mut cells = row.cells.into_iter().map(|cell| cell.text.trim().to_string()).collect::<Vec<_>>();
                    cells.resize(width, String::new());
                    cells
                }).collect();
                tables.push(ExtractedTable { page: page_index + 1, table: table_index + 1, rows });
            }
            continue;
        }

        // === 2. 扫描页：需要 OCR ===
        // 延迟初始化 OCR 引擎（只在遇到扫描页时才加载）
        if ocr_engine.is_none() {
            let cache = app.path().app_cache_dir().map_err(|e| format!("无法定位 OCR 模型缓存目录：{e}"))?.join("paddleocr-chinese");
            std::fs::create_dir_all(&cache).map_err(|e| format!("无法创建 OCR 模型缓存目录：{e}"))?;
            let spec = pdf_oxide::extractors::auto::OcrLanguage::Chinese.spec();
            let models = [
                ("det.onnx", pdf_oxide::extractors::auto::OcrLanguage::DET_URL),
                (spec.rec_file.as_str(), spec.rec_url.as_str()),
                (spec.dict_file.as_str(), spec.dict_url.as_str()),
            ];
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
            let ocr_config = build_ocr_config();
            ocr_engine = Some(OcrEngine::new(det, rec, dict, ocr_config)
                .map_err(|e| format!("加载中文 OCR 模型失败：{e}"))?);

            // 初始化渲染选项 —— 300 DPI 是 OCR 黄金分辨率
            let mut ro = RenderOptions::with_dpi(300);
            ro.format = ImageFormat::Png;
            ro.background = Some([1.0, 1.0, 1.0, 1.0]);
            render_options = Some(ro);
        }

        let engine = match ocr_engine.as_ref() {
            Some(e) => e,
            None => continue,
        };

        // 获取 PDF 页面实际尺寸（pt），用于坐标映射
        // 尝试从原 document 获取 page size
        // 这里用一个更可靠的方式：先渲染再从渲染结果反推

        // === OCR 管线：优先整页渲染，回退嵌入图像 ===
        let mut all_spans = Vec::new();
        let mut page_known = false;

        // 策略 A：整页 300 DPI 渲染 → OCR（最可靠，主路径）
        if let Some(ref ro) = render_options {
            match render_page(&document, page_index, ro) {
                Ok(rendered) => {
                    // 从渲染结果反推 PDF 页面在 pt 下的尺寸
                    // rendered.width_px / dpi * 72 = pt
                    let dpi = ro.dpi as f32;
                    let estimated_width_pt = rendered.width as f32 / dpi * 72.0;
                    let estimated_height_pt = rendered.height as f32 / dpi * 72.0;
                    page_width_pt = estimated_width_pt;
                    page_height_pt = estimated_height_pt;
                    page_known = true;

                    // 解码 PNG 到 DynamicImage
                    let format = match rendered.format {
                        ImageFormat::Png => image::ImageFormat::Png,
                        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
                        _ => { eprintln!("第 {} 页：不支持的渲染格式，跳过整页渲染 OCR", page_index + 1); continue; }
                    };
                    let raster = match image::load_from_memory_with_format(&rendered.data, format) {
                        Ok(img) => img,
                        Err(e) => { eprintln!("第 {} 页解码渲染图像失败：{e}", page_index + 1); continue; }
                    };

                    let recognized = match engine.ocr_image(&raster) {
                        Ok(r) => r,
                        Err(e) => { eprintln!("第 {} 页整页渲染 OCR 失败：{e}", page_index + 1); continue; }
                    };

                    // 把 OCR 坐标从渲染像素映射回 PDF pt 空间
                    let scale_x = estimated_width_pt / rendered.width as f32;
                    let scale_y = estimated_height_pt / rendered.height as f32;
                    for mut span in recognized.to_text_spans(1.0) {
                        span.bbox.x *= scale_x;
                        span.bbox.y = estimated_height_pt - span.bbox.y * scale_y; // OCR 的 y 是从上到下，PDF 从下到上
                        span.bbox.width *= scale_x;
                        span.bbox.height *= scale_y;
                        span.font_size *= (scale_x + scale_y) / 2.0;
                        all_spans.push(span);
                    }

                    if !all_spans.is_empty() {
                        ocr_used = true;
                    }
                }
                Err(e) => eprintln!("第 {} 页整页渲染警告：{e}（将尝试嵌入图像）", page_index + 1),
            }
        }

        // 策略 B：如果整页渲染没产出 spans，尝试嵌入图像
        if all_spans.is_empty() {
            if let Ok(images) = document.extract_images(page_index) {
                for image in images {
                    let Some(bbox) = image.bbox() else { continue };
                    if image.width() == 0 || image.height() == 0 || bbox.width <= 0.0 || bbox.height <= 0.0 { continue; }

                    let raster = match image.to_dynamic_image() {
                        Ok(r) => r,
                        Err(e) => { eprintln!("第 {} 页解码嵌入图像失败：{e}", page_index + 1); continue; }
                    };

                    let recognized = match engine.ocr_image(&raster) {
                        Ok(r) => r,
                        Err(e) => { eprintln!("第 {} 页嵌入图像 OCR 失败：{e}", page_index + 1); continue; }
                    };

                    let scale_x = bbox.width / image.width() as f32;
                    let scale_y = bbox.height / image.height() as f32;
                    for mut span in recognized.to_text_spans(1.0) {
                        span.bbox.x = bbox.x + span.bbox.x * scale_x;
                        span.bbox.y = bbox.y + bbox.height - span.bbox.y * scale_y; // 翻转 y 轴
                        span.bbox.width *= scale_x;
                        span.bbox.height *= scale_y;
                        span.font_size *= (scale_x + scale_y) / 2.0;
                        all_spans.push(span);
                    }
                    page_width_pt = page_width_pt.max(bbox.x + bbox.width);
                    page_height_pt = page_height_pt.max(bbox.y + bbox.height);
                    page_known = true;
                }
            }
        }

        if !all_spans.is_empty() {
            ocr_used = true;
            // 用更宽松的配置检测 OCR 结果中的表格
            let detector = SpatialTableDetector::with_config(TableDetectionConfig::relaxed());
            let page_tables = detector.detect_tables_hybrid(&all_spans, &[]);
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

        let _ = page_known; // 抑制未使用警告
    }

    Ok(ExtractionResult { page_count, tables, pages_without_text, ocr_used })
}
