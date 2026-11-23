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

/// 构建优化后的 OCR 配置：偏向高召回，避免表格单元格文字被整行丢弃
fn build_ocr_config() -> pdf_oxide::ocr::OcrConfig {
    let mut config = pdf_oxide::ocr::OcrConfig::default();
    // 降低文本检测阈值 — 扫描件中弱对比度、细笔画的表格文字也能被框出来
    config.det_threshold = 0.25;
    // 略降低检测框置信度阈值 — 留住贴边、被表格线切割的文本块
    config.box_threshold = 0.5;
    // 识别置信度保持较低水平 — 宁可保留待人工校正的疑似结果，也不整行丢弃造成空单元格
    config.rec_threshold = 0.4;
    config
}

/// 把 OCR 输出转换为 PDF 点位坐标系下的文本块。
///
/// 这里不使用 `to_text_spans()`：它在构造 `Rect` 时把 max_x/max_y 当成了宽高，
/// 会让每个文本块尺寸虚胖，进而干扰列对齐。改用 `bounding_rect()` 自行换算。
fn ocr_output_to_spans(
    output: &pdf_oxide::ocr::OcrOutput,
    pt_per_px: f32,
    offset: (f32, f32),
) -> Vec<pdf_oxide::layout::text_block::TextSpan> {
    let mut spans: Vec<&pdf_oxide::ocr::OcrSpan> = output.spans.iter().collect();
    // 阅读顺序：先按 10px 为区间自上而下，再自左而右
    spans.sort_by(|a, b| {
        let ra = a.bounding_rect();
        let rb = b.bounding_rect();
        ((ra.y / 10.0) as i64)
            .cmp(&((rb.y / 10.0) as i64))
            .then(ra.x.total_cmp(&rb.x))
            .then(ra.y.total_cmp(&rb.y))
    });
    spans
        .iter()
        .enumerate()
        .map(|(sequence, span)| {
            let rect = span.bounding_rect();
            let mut text_span = span.to_text_span(sequence, 1.0);
            text_span.bbox = pdf_oxide::geometry::Rect::new(
                offset.0 + rect.x * pt_per_px,
                offset.1 + rect.y * pt_per_px,
                rect.width * pt_per_px,
                rect.height * pt_per_px,
            );
            // 文本块高度约等于字号，去掉上下留白后的经验系数
            text_span.font_size = (rect.height * pt_per_px * 0.75).clamp(6.0, 72.0);
            text_span
        })
        .collect()
}

/// 整页高分辨率渲染后 OCR。
/// 扫描件整页通常只有一张大图，按页渲染比单独抠嵌入图更完整，也不会漏掉跨图内容。
fn ocr_rendered_page(
    document: &pdf_oxide::PdfDocument,
    page_index: usize,
    options: &pdf_oxide::rendering::RenderOptions,
    engine: &pdf_oxide::ocr::OcrEngine,
) -> Result<Vec<pdf_oxide::layout::text_block::TextSpan>, String> {
    use pdf_oxide::rendering::ImageFormat;

    let rendered = pdf_oxide::rendering::render_page(document, page_index, options)
        .map_err(|e| format!("渲染页面失败：{e}"))?;
    if rendered.width == 0 || rendered.height == 0 {
        return Err("渲染结果尺寸为空".to_string());
    }
    let format = match rendered.format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        other => return Err(format!("不支持的渲染格式：{other:?}")),
    };
    let raster = image::load_from_memory_with_format(&rendered.data, format)
        .map_err(|e| format!("解码渲染图像失败：{e}"))?;

    // 用页面可见区域反推「像素 → 点位」比例，兼容被 max_output_pixels 限流降采样的情况
    let (bx, by, bx2, by2) = document
        .get_page_visible_box(page_index)
        .map_err(|e| format!("读取页面尺寸失败：{e}"))?;
    let box_width_pt = (bx2 - bx).abs().max(1.0);
    let pt_per_px = box_width_pt / rendered.width as f32;

    let output = engine.ocr_image(&raster).map_err(|e| format!("OCR 识别失败：{e}"))?;
    Ok(ocr_output_to_spans(&output, pt_per_px, (bx.min(bx2), by.min(by2))))
}

/// 回退路径：直接对页面内的嵌入图像做 OCR（适用于渲染失败或图像本身分辨率更高的 PDF）
fn ocr_embedded_images(
    document: &pdf_oxide::PdfDocument,
    page_index: usize,
    engine: &pdf_oxide::ocr::OcrEngine,
) -> Result<Vec<pdf_oxide::layout::text_block::TextSpan>, String> {
    let images = document
        .extract_images(page_index)
        .map_err(|e| format!("读取扫描图片失败：{e}"))?;
    let mut all = Vec::new();
    for image in images {
        let Some(bbox) = image.bbox() else { continue };
        if image.width() == 0 || image.height() == 0 || bbox.width <= 0.0 || bbox.height <= 0.0 {
            continue;
        }
        let raster = image
            .to_dynamic_image()
            .map_err(|e| format!("解码扫描图片失败：{e}"))?;
        let output = engine.ocr_image(&raster).map_err(|e| format!("OCR 识别失败：{e}"))?;
        let pt_per_px = bbox.width / image.width() as f32;
        all.extend(ocr_output_to_spans(&output, pt_per_px, (bbox.x, bbox.y)));
    }
    Ok(all)
}

/// 列区间聚类中的一列
struct Column {
    /// 首个成员区间，作为比较基准（避免宽单元格吞并相邻列）
    core_x0: f32,
    core_x1: f32,
    x0: f32,
    x1: f32,
    count: usize,
}

/// 按区间重叠把文本块聚成列
fn detect_table_columns(spans: &[&pdf_oxide::layout::text_block::TextSpan]) -> Vec<Column> {
    let mut items: Vec<(f32, f32)> = spans
        .iter()
        .map(|span| (span.bbox.x, span.bbox.x + span.bbox.width))
        .filter(|(x0, x1)| x1 > x0)
        .collect();
    items.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut columns: Vec<Column> = Vec::new();
    for (x0, x1) in items {
        let width = (x1 - x0).max(0.1);
        let mut best: Option<usize> = None;
        let mut best_ratio = 0.0f32;
        for (index, column) in columns.iter().enumerate() {
            let overlap = (x1.min(column.core_x1) - x0.max(column.core_x0)).max(0.0);
            let core_width = (column.core_x1 - column.core_x0).max(0.1);
            let ratio = overlap / width.min(core_width);
            if ratio > best_ratio {
                best_ratio = ratio;
                best = Some(index);
            }
        }
        match best {
            Some(index) if best_ratio >= 0.6 => {
                let column = &mut columns[index];
                column.x0 = column.x0.min(x0);
                column.x1 = column.x1.max(x1);
                column.count += 1;
            }
            _ => columns.push(Column { core_x0: x0, core_x1: x1, x0, x1, count: 1 }),
        }
    }
    columns.sort_by(|a, b| a.x0.total_cmp(&b.x0));
    // 只出现一次的窄文本（如大标题）不算列
    columns.into_iter().filter(|column| column.count >= 2).collect()
}

/// 用「行聚类 + 列区间聚类」把 OCR 文本块重建成表格网格。
/// OCR 结果没有表格线可依赖，库自带的线性检测会把一张表切成多块，这里自己重建。
fn reconstruct_table_grid(
    spans: &[pdf_oxide::layout::text_block::TextSpan],
) -> Option<Vec<Vec<String>>> {
    if spans.len() < 6 {
        return None;
    }
    let refs: Vec<&pdf_oxide::layout::text_block::TextSpan> = spans.iter().collect();

    // 1) 行聚类：按 y 中心聚合，容差取文本高度的一半左右
    let mut sorted = refs.clone();
    sorted.sort_by(|a, b| a.bbox.y.total_cmp(&b.bbox.y));
    let mut heights: Vec<f32> = sorted
        .iter()
        .map(|span| span.bbox.height)
        .filter(|height| *height > 0.5)
        .collect();
    heights.sort_by(f32::total_cmp);
    let median_height = heights.get(heights.len() / 2).copied().unwrap_or(10.0);
    let tolerance = (median_height * 0.6).clamp(2.0, 8.0);

    let mut row_spans: Vec<Vec<&pdf_oxide::layout::text_block::TextSpan>> = Vec::new();
    let mut centers: Vec<f32> = Vec::new();
    for span in sorted {
        let center = span.bbox.y + span.bbox.height / 2.0;
        if centers.last().map(|last| (center - last).abs() <= tolerance).unwrap_or(false) {
            let row = row_spans.last_mut().unwrap();
            row.push(span);
            let count = row.len() as f32;
            let last = centers.last_mut().unwrap();
            *last += (center - *last) / count;
        } else {
            row_spans.push(vec![span]);
            centers.push(center);
        }
    }

    // 2) 列聚类
    let columns = detect_table_columns(&refs);
    if columns.len() < 2 {
        return None;
    }

    // 3) 逐行填充单元格：优先按区间重叠归属，其次按中心距离
    let mut grid: Vec<Vec<String>> = Vec::new();
    for row in &row_spans {
        let mut cells = vec![String::new(); columns.len()];
        let mut ordered = row.clone();
        ordered.sort_by(|a, b| a.bbox.x.total_cmp(&b.bbox.x));
        for span in ordered {
            let center = span.bbox.x + span.bbox.width / 2.0;
            let mut best_index = 0;
            let mut best_score = f32::MIN;
            for (index, column) in columns.iter().enumerate() {
                let overlap = (span.bbox.x + span.bbox.width).min(column.x1) - span.bbox.x.max(column.x0);
                let column_center = (column.x0 + column.x1) / 2.0;
                let score = if overlap > 0.0 { overlap } else { -((center - column_center).abs()) };
                if score > best_score {
                    best_score = score;
                    best_index = index;
                }
            }
            let text = span.text.trim();
            if text.is_empty() {
                continue;
            }
            if !cells[best_index].is_empty() {
                cells[best_index].push(' ');
            }
            cells[best_index].push_str(text);
        }
        grid.push(cells);
    }

    // 4) 单元格内换行产生的单格行，并回上一行同列
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row in grid {
        let filled: Vec<usize> = row
            .iter()
            .enumerate()
            .filter(|(_, cell)| !cell.is_empty())
            .map(|(index, _)| index)
            .collect();
        let can_merge = filled.len() == 1
            && rows.last().map(|prev| !prev[filled[0]].is_empty()).unwrap_or(false);
        if can_merge {
            let prev = rows.last_mut().unwrap();
            prev[filled[0]].push(' ');
            prev[filled[0]].push_str(&row[filled[0]]);
        } else {
            rows.push(row);
        }
    }

    // 5) 去掉全空行与全空列
    rows.retain(|row| row.iter().any(|cell| !cell.is_empty()));
    let mut keep = vec![true; columns.len()];
    for (index, item) in keep.iter_mut().enumerate() {
        if rows.iter().all(|row| row[index].trim().is_empty()) {
            *item = false;
        }
    }
    let rows: Vec<Vec<String>> = rows
        .into_iter()
        .map(|row| row.into_iter().enumerate().filter(|(index, _)| keep[*index]).map(|(_, cell)| cell).collect())
        .collect();

    if rows.len() < 2 { None } else { Some(rows) }
}


#[tauri::command]
fn extract_pdf_tables(app: tauri::AppHandle, path: String) -> Result<ExtractionResult, String> {
    use pdf_oxide::ocr::OcrEngine;
    use pdf_oxide::rendering::{ImageFormat, RenderOptions};
    use pdf_oxide::structure::spatial_table_detector::{SpatialTableDetector, TableDetectionConfig};
    use std::io::Read;
    use tauri::Manager;

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

    let mut tables = Vec::new();
    let mut ocr_used = false;
    let mut ocr_engine: Option<OcrEngine> = None;
    let mut render_options: Option<RenderOptions> = None;

    for page_index in 0..page_count {
        let is_scanned = pages_without_text.contains(&(page_index + 1));

        // 非扫描页：直接走原生表格提取
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

        // 扫描页：延迟初始化 OCR 引擎（只在真正需要时才加载模型）
        if ocr_engine.is_none() {
            let cache = app.path().app_cache_dir()
                .map_err(|e| format!("无法定位 OCR 模型缓存目录：{e}"))?
                .join("paddleocr-chinese");
            std::fs::create_dir_all(&cache).map_err(|e| format!("无法创建 OCR 模型缓存目录：{e}"))?;
            let spec = pdf_oxide::extractors::auto::OcrLanguage::Chinese.spec();
            let models = [
                ("det.onnx", pdf_oxide::extractors::auto::OcrLanguage::DET_URL),
                (spec.rec_file.as_str(), spec.rec_url.as_str()),
                (spec.dict_file.as_str(), spec.dict_url.as_str()),
            ];
            // 模型优先从资源目录复制，缺失时联网下载兜底
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
            ocr_engine = Some(OcrEngine::new(det, rec, dict, build_ocr_config())
                .map_err(|e| format!("加载中文 OCR 模型失败：{e}"))?);

            // 300 DPI 是 OCR 的黄金分辨率：表格小字足够清晰，体积和耗时也可控
            let mut options = RenderOptions::with_dpi(300);
            options.format = ImageFormat::Png;
            options.background = Some([1.0, 1.0, 1.0, 1.0]);
            render_options = Some(options);
        }

        let engine = match ocr_engine.as_ref() {
            Some(engine) => engine,
            None => continue,
        };

        // 主路径：整页渲染 + OCR；失败或为空时回退到嵌入图像
        let mut spans = Vec::new();
        if let Some(options) = render_options.as_ref() {
            match ocr_rendered_page(&document, page_index, options, engine) {
                Ok(result) if !result.is_empty() => spans = result,
                Ok(_) => eprintln!("第 {} 页整页 OCR 未识别到文字，尝试嵌入图像", page_index + 1),
                Err(e) => eprintln!("第 {} 页整页渲染 OCR 失败：{e}（回退到嵌入图像）", page_index + 1),
            }
        }
        if spans.is_empty() {
            match ocr_embedded_images(&document, page_index, engine) {
                Ok(result) => spans = result,
                Err(e) => eprintln!("第 {} 页嵌入图像 OCR 失败：{e}", page_index + 1),
            }
        }

        if spans.is_empty() {
            continue;
        }
        ocr_used = true;

        // OCR 结果没有表格线可依赖，优先用自研的行/列聚类重建整页网格；
        // 库自带的线性检测会按页面列带把一张表切成多个碎片。
        if let Some(rows) = reconstruct_table_grid(&spans) {
            tables.push(ExtractedTable { page: page_index + 1, table: 1, rows });
            continue;
        }

        // 重建失败（文本过少或列数不足）时，退回库的宽松空间检测
        let detector = SpatialTableDetector::with_config(TableDetectionConfig::relaxed());
        let page_tables = detector.detect_tables_hybrid(&spans, &[]);
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
