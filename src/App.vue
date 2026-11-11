<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { open, save } from "@tauri-apps/plugin-dialog";
import { readFile, writeFile } from "@tauri-apps/plugin-fs";
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { invoke } from "@tauri-apps/api/core";
import * as XLSX from "xlsx";
import * as pdfjs from "pdfjs-dist/legacy/build/pdf.js";
import workerUrl from "pdfjs-dist/legacy/build/pdf.worker.min.js?url";

type PdfItem = { name: string; path: string; bytes?: Uint8Array; pages: number; state: string };
type Engine = "rasgapdf" | "unpdf" | "pdf-inspector" | "pdf_oxide";
type TableResult = { page: number; table: number; rows: string[][] };
type ExtractionResult = { pageCount: number; tables: TableResult[]; pagesWithoutText: number[]; ocrUsed: boolean };
pdfjs.GlobalWorkerOptions.workerSrc = workerUrl;
const engines: { id: Engine; name: string; detail: string }[] = [
  { id: "rasgapdf", name: "rasgapdf", detail: "结构化表格识别" },
  { id: "unpdf", name: "unpdf", detail: "文本与页面分析" },
  { id: "pdf-inspector", name: "pdf-inspector", detail: "版面检查识别" },
  { id: "pdf_oxide", name: "pdf_oxide", detail: "原生 PDF 解析" },
];
const files = ref<PdfItem[]>([]);
const selectedIndex = ref(0);
const engine = ref<Engine>("pdf_oxide");
const page = ref(1);
const preview = ref<HTMLCanvasElement | null>(null);
const busy = ref(false);
const notice = ref("");
const update = ref<Awaited<ReturnType<typeof check>> | null>(null);
const current = computed(() => files.value[selectedIndex.value]);
const rows = ref<string[][]>([["列 1", "列 2", "列 3"], ["", "", "" ]]);
const extracted = ref<Record<string, TableResult[]>>({});
const activeTable = ref(0);

async function choosePdfs() {
  const selected = await open({ multiple: true, filters: [{ name: "PDF 文件", extensions: ["pdf"] }] });
  if (!selected) return;
  const paths = Array.isArray(selected) ? selected : [selected];
  const added: PdfItem[] = [];
  for (const path of paths) {
    if (files.value.some((f) => f.path === path)) continue;
    const name = path.split(/[\\/]/).pop() || path;
    const item = { name, path, pages: 0, state: "等待识别" };
    files.value.push(item);
    added.push(item);
  }
  selectedIndex.value = Math.max(0, files.value.indexOf(added[0] ?? files.value[0]));
  await loadCurrent();
  if (added.length) await recognize(added);
}
async function loadCurrent() {
  const item = current.value;
  if (!item) return;
  try {
    const bytes = await readFile(item.path);
    item.bytes = bytes;
    const doc = await pdfjs.getDocument({ data: bytes.slice() }).promise;
    item.pages = doc.numPages;
    page.value = 1;
    await renderPage();
  } catch (e) { notice.value = `PDF 页面预览失败：${String(e)}`; }
}
async function renderPage() {
  const bytes = current.value?.bytes;
  if (!bytes || !preview.value) return;
  try {
    const doc = await pdfjs.getDocument({ data: bytes.slice() }).promise;
    page.value = Math.max(1, Math.min(page.value, doc.numPages));
    const p = await doc.getPage(page.value);
    const vp = p.getViewport({ scale: Math.min(1.6, 700 / p.getViewport({ scale: 1 }).width) });
    const canvas = preview.value;
    canvas.width = vp.width; canvas.height = vp.height;
    const context = canvas.getContext("2d");
    if (!context) return;
    await p.render({ canvasContext: context, viewport: vp }).promise;
  } catch (e) { notice.value = String(e); }
}
async function recognize(targetFiles: PdfItem[] = current.value ? [current.value] : []) {
  if (!targetFiles.length) return;
  busy.value = true;
  try {
    if (engine.value !== "pdf_oxide") throw new Error("目前已接入 pdf_oxide。请选择 pdf_oxide 识别方式。");
    let total = 0;
    for (const file of targetFiles) {
      if (extracted.value[file.path]) { total += extracted.value[file.path].length; continue; }
      file.state = "正在解析（扫描件会使用本地中文 OCR）…";
      try {
        const result = await invoke<ExtractionResult>("extract_pdf_tables", { path: file.path });
        file.pages = result.pageCount;
        extracted.value[file.path] = result.tables;
        file.state = result.tables.length ? `识别到 ${result.tables.length} 个表格` : result.ocrUsed ? "OCR完成，未识别到表格" : result.pagesWithoutText.length ? "扫描件，OCR未提取文字" : "未检测到表格";
        total += result.tables.length;
        if (!result.tables.length && result.pagesWithoutText.length) {
          notice.value = result.ocrUsed
            ? `${file.name} 已通过本地中文 OCR 提取文字，但没有重建出表格。可以检查页面图像质量，或在结果区手动整理后导出。`
            : `${file.name} 的扫描页未能通过 OCR 提取文字。首次识别会自动下载开源中文模型并保存在本机缓存；请检查网络后重试。`;
        }
      } catch (e) { file.state = "识别失败"; throw new Error(`${file.name}：${String(e)}`); }
    }
    if (current.value && targetFiles.some((file) => file.path === current.value?.path)) {
      activeTable.value = 0;
      loadResultTable();
    }
    if (!targetFiles.some((file) => file.state.includes("OCR完成") || file.state.includes("OCR未提取"))) notice.value = `解析完成，共找到 ${total} 个表格。`;
  } catch (e) { notice.value = String(e); }
  finally { busy.value = false; }
}
function persistResultTable() {
  if (!current.value) return;
  const list = extracted.value[current.value.path];
  if (list?.[activeTable.value]) list[activeTable.value].rows = rows.value.map((row) => [...row]);
}
function loadResultTable() {
  if (!current.value) return;
  const list = extracted.value[current.value.path] ?? [];
  activeTable.value = Math.min(activeTable.value, Math.max(0, list.length - 1));
  const table = list[activeTable.value];
  rows.value = table?.rows.map((row) => [...row]) ?? [["", "", ""]];
}
function selectFile(index: number) { persistResultTable(); selectedIndex.value = index; activeTable.value = 0; loadCurrent(); loadResultTable(); if (!extracted.value[files.value[index]?.path]) recognize([files.value[index]]); }
function selectTable(index: number) { persistResultTable(); activeTable.value = index; loadResultTable(); }
function addRow() { rows.value.push(Array(Math.max(1, rows.value[0]?.length || 1)).fill("")); }
function removeRow(index: number) { if (rows.value.length > 1) rows.value.splice(index, 1); }
async function exportExcel() {
  if (!files.value.length) { notice.value = "请先导入 PDF 文件。"; return; }
  const path = await save({ defaultPath: "PDF表格.xlsx", filters: [{ name: "Excel 工作簿", extensions: ["xlsx"] }] });
  if (!path) return;
  try {
    const book = XLSX.utils.book_new();
    persistResultTable();
    for (const file of files.value) {
      const tables = extracted.value[file.path] ?? [];
      if (tables.length) {
        for (const item of tables) {
          const sheet = XLSX.utils.aoa_to_sheet(item.rows);
          const base = file.name.replace(/\.pdf$/i, "").slice(0, 20) || "PDF";
          XLSX.utils.book_append_sheet(book, sheet, `${base}_P${item.page}_T${item.table}`.slice(0, 31));
        }
      } else if (file === current.value) {
        XLSX.utils.book_append_sheet(book, XLSX.utils.aoa_to_sheet(rows.value), file.name.replace(/\.pdf$/i, "").slice(0, 31) || "Sheet");
      }
    }
    if (!book.SheetNames.length) throw new Error("没有可导出的表格，请先使用 pdf_oxide 执行识别。");
    const data = XLSX.write(book, { type: "array", bookType: "xlsx" });
    await writeFile(path, new Uint8Array(data));
    notice.value = `已保存 Excel：${path}`;
  } catch (e) { notice.value = `保存失败：${String(e)}`; }
}
async function checkUpdate() { try { update.value = await check(); } catch { /* updater is configured by the release owner */ } }
async function installUpdate() { if (!update.value) return; await update.value.downloadAndInstall(); await relaunch(); }
onMounted(checkUpdate);
</script>

<template>
  <div class="app">
    <aside class="sidebar">
      <div class="brand"><div class="brand-icon">表</div><div><strong>PDF 表格工坊</strong><small>PDF → Excel</small></div></div>
      <div class="nav-caption">工作区</div><button class="nav active"><span>▦</span>表格转换</button>
      <div class="side-note"><div class="spark">✳</div><strong>本地处理，文件不上传</strong><p>预览由 PDF.js 提供。表格识别由你选择的引擎处理。</p></div>
      <footer><span class="status-dot"></span>准备就绪 <span class="version">v0.1.0</span></footer>
    </aside>
    <main>
      <header><div><span class="crumb">工作区　/　PDF 表格转换</span><h1>把 PDF 表格变成 Excel</h1><p>导入多个文件，选择识别器，检查结果并保存。</p></div><button class="outline" @click="choosePdfs">＋ 导入 PDF</button></header>
      <div v-if="update" class="update">发现新版本 {{ update.version }} <button @click="installUpdate">立即更新</button></div>
      <div class="workspace">
        <section class="left-col">
          <div class="card files-card"><div class="card-title"><div><span class="step">01</span><strong>PDF 文件</strong></div><button class="text-btn" @click="choosePdfs">＋ 添加文件</button></div>
            <div v-if="!files.length" class="dropzone" @click="choosePdfs"><div class="upload-icon">↑</div><strong>点击选择 PDF 文件</strong><span>支持同时导入多个文件</span><small>PDF</small></div>
            <div v-else class="file-list"><div v-for="(f,i) in files" :key="f.path" :class="['file-row',{chosen:selectedIndex===i}]" role="button" tabindex="0" @click="selectFile(i)" @keyup.enter="selectFile(i)"><span class="pdf-icon">PDF</span><span class="file-info"><strong>{{ f.name }}</strong><small>{{ f.pages ? `${f.pages} 页` : '读取中…' }}</small></span><span class="file-state">{{ f.state }}</span><button class="remove" @click.stop="files.splice(i,1);selectedIndex=0;loadResultTable()">×</button></div></div>
          </div>
          <div class="card engine-card"><div class="card-title"><div><span class="step">02</span><strong>识别方式</strong></div><span class="hint">已接入 pdf_oxide</span></div><div class="engine-list"><button v-for="e in engines" :key="e.id" :class="['engine-option',{selected:engine===e.id,disabled:e.id!=='pdf_oxide'}]" @click="e.id==='pdf_oxide'&&(engine=e.id)"><span class="radio"></span><span><strong>{{ e.name }}</strong><small>{{ e.id==='pdf_oxide' ? 'Rust 表格识别（已接入）' : '后续接入' }}</small></span><span class="engine-tag">{{ e.id==='pdf_oxide' ? '可用' : '未接入' }}</span></button></div></div>
          <button class="recognize" :disabled="!files.length||busy" @click="recognize()"><span>✳</span>{{ busy ? '正在识别…' : `使用 ${engine} 开始识别` }}<b>→</b></button>
        </section>
        <section class="right-col"><div class="card preview-card"><div class="card-title"><div><span class="step">预览</span><strong>{{ current?.name || 'PDF 页面' }}</strong></div><div class="page-controls"><button :disabled="page<=1" @click="page--;renderPage()">‹</button><span>{{ page }} / {{ current?.pages || 0 }}</span><button :disabled="page>= (current?.pages||0)" @click="page++;renderPage()">›</button></div></div><div class="pdf-view"><div v-if="!current" class="preview-empty"><div>▤</div><strong>导入 PDF 开始预览</strong><span>页面内容只用于查看</span></div><canvas v-show="current" ref="preview"></canvas></div></div>
          <div class="card result-card"><div class="card-title"><div><span class="step">03</span><strong>识别结果</strong></div><div><select v-if="(extracted[current?.path||'']||[]).length>1" :value="activeTable" @change="selectTable(Number(($event.target as HTMLSelectElement).value))"><option v-for="(t,i) in extracted[current?.path||'']" :key="i" :value="i">第 {{ t.page }} 页 · 表格 {{ t.table }}</option></select><button class="text-btn" @click="addRow">＋ 添加行</button></div></div><div class="table-wrap"><table><tbody><tr v-for="(row,ri) in rows" :key="ri"><td v-for="(_,ci) in row" :key="ci"><input v-model="rows[ri][ci]" :placeholder="ri===0?`列 ${ci+1}`:'—'"></td><td class="row-action"><button @click="removeRow(ri)">×</button></td></tr></tbody></table></div><div class="result-foot"><span>可直接编辑表格内容</span><button class="save-btn" @click="exportExcel">↓　保存为 Excel</button></div></div>
        </section>
      </div>
      <div v-if="notice" class="toast"><span>{{ notice }}</span><button @click="notice=''">×</button></div>
    </main>
  </div>
</template>
