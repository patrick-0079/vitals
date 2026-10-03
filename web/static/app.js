/* vitals web 壳前端 —— 无构建、无依赖，只消费 /ws 推来的数据契约。
 * 契约说明见 /api/contract，或 web/server.py 顶部注释。 */

const $ = (id) => document.getElementById(id);

const esc = (s) => String(s).replace(/[&<>"']/g,
  (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

/** 数值格式化：null/undefined/NaN 统一渲染成 n/a（契约里「拿不到」就是 null） */
function n(v, digits = 0, suffix = "") {
  if (v === null || v === undefined || (typeof v === "number" && !Number.isFinite(v))) {
    return '<span class="na">n/a</span>';
  }
  return `${Number(v).toFixed(digits)}${suffix}`;
}

const isNum = (v) => typeof v === "number" && Number.isFinite(v);

function sevClass(pct, warn = 75, bad = 90) {
  if (!isNum(pct)) return "";
  return pct >= bad ? "bad" : pct >= warn ? "warn" : "ok";
}

function setBar(id, pct, cls) {
  const el = $(id);
  if (!el) return;
  const p = isNum(pct) ? Math.max(0, Math.min(100, pct)) : 0;
  el.style.width = p + "%";
  el.className = "fill " + (cls || sevClass(pct));
}

function setText(id, html) {
  const el = $(id);
  if (el) el.innerHTML = html;
}

/* ------------------------------------------------------------------ CPU */

function renderCpu(cpu) {
  setText("cpu-usage", n(cpu.usage_pct, 1, "%"));
  setBar("cpu-usage-bar", cpu.usage_pct);
  setText("cpu-temp", isNum(cpu.temp_c) ? `${cpu.temp_c.toFixed(1)}°C` : n(null));
  const ccd = (cpu.ccd_temps_c || []).filter(isNum);
  setText("cpu-ccd", ccd.length ? "CCD " + ccd.map((t) => t.toFixed(1) + "°C").join(" / ") : "");
  setText("cpu-source", esc(cpu.temp_source || ""));
  setText("cpu-freq", `${cpu.freq_mhz || 0} MHz`);
  setText("cpu-power", isNum(cpu.package_power_w) ? `${cpu.package_power_w.toFixed(1)} W` : n(null));
  const volt = [];
  if (isNum(cpu.core_voltage_v)) volt.push("Vcore " + cpu.core_voltage_v.toFixed(4) + " V");
  if (isNum(cpu.soc_voltage_v)) volt.push("SoC " + cpu.soc_voltage_v.toFixed(4) + " V");
  setText("cpu-volt", volt.join("   "));

  const per = cpu.per_core_pct || [];
  setText("cpu-cores-count", per.length ? `${per.length} 线程` : "");
  $("cpu-coregrid").innerHTML = per.map((p, i) => `
    <div class="corecell">
      <div class="n">#${i}</div>
      <div class="v">${n(p, 0, "%")}</div>
      <div class="bar"><div class="fill ${sevClass(p)}" style="width:${isNum(p) ? Math.min(100, p) : 0}%"></div></div>
    </div>`).join("");

  const cores = cpu.per_core || [];
  $("cpu-percore").innerHTML = cores.length
    ? cores.map((c) => `
      <div class="pcore">
        <span class="idx">#${c.index}</span>
        <span class="dim"> t${c.thread} </span>
        ${n(c.clock_mhz, 0)}<span class="dim">/</span>${n(c.effective_mhz, 0)} MHz
        &nbsp;${isNum(c.power_w) ? c.power_w.toFixed(1) + " W" : '<span class="na">n/a</span>'}
        ${isNum(c.aperf_mperf_ratio) ? `<span class="dim"> ×${c.aperf_mperf_ratio.toFixed(3)}</span>` : ""}
      </div>`).join("")
    : '<div class="gauge-sub">需要管理员权限（MSR / PawnIO 只对管理员开放）</div>';

  const smu = cpu.smu || [];
  $("cpu-smu").innerHTML = smu.length
    ? smu.map((s) => `<span class="chip ${esc(s.kind)}"><span class="k">${esc(s.name)}</span>${n(s.value, s.kind === "clock" ? 0 : 2)} <span class="k">${esc(s.unit)}</span></span>`).join("")
    : '<span class="chip off-source">SMU PM 表不可用（CPU 代号/表版本没有已知布局）</span>';
}

/* --------------------------------------------------------------- 内存 */

function renderMemory(mem) {
  setText("mem-usage", n(mem.usage_pct, 1, "%"));
  setBar("mem-bar", mem.usage_pct);
  setText("mem-detail", `已用 ${n(mem.used_gb, 2)} GB / 共 ${n(mem.total_gb, 2)} GB`);
}

function renderDimms(dimms) {
  $("dimms").innerHTML = (dimms || []).length
    ? dimms.map((d) => {
        const bad = d.thermal_status && d.thermal_status !== "Good";
        return `
        <div class="dimm">
          <div class="pn">0x${(d.address ?? 0).toString(16).toUpperCase().padStart(2, "0")} &nbsp;${esc(d.part_number || "")}</div>
          <div class="temp">${isNum(d.temp_c) ? d.temp_c.toFixed(2) + "°C" : '<span class="na">n/a</span>'}
            ${bad ? `<span class="warn"> ${esc(d.thermal_status)}</span>` : ""}</div>
          <div class="r">${esc(d.serial_number || "")} · ${esc(d.manufacturer || "")}${d.manufacture_date ? " · " + esc(d.manufacture_date) : ""}</div>
          <div class="r">${esc(d.source || "")}</div>
        </div>`;
      }).join("")
    : '<div class="gauge-sub">没有识别到 DDR5 模组（需要管理员权限读 SMBus SPD）</div>';
}

/* ---------------------------------------------------------------- GPU */

function renderGpu(gpu) {
  $("gpu-none").classList.toggle("hidden", !!gpu);
  $("gpu-body").classList.toggle("hidden", !gpu);
  if (!gpu) return;

  setText("gpu-source", esc(gpu.source || ""));
  setText("gpu-usage", n(gpu.usage_pct, 1, "%"));
  setBar("gpu-usage-bar", gpu.usage_pct);
  setText("gpu-temp", isNum(gpu.temp_c) ? `${gpu.temp_c.toFixed(0)}°C` : n(null));

  const tsub = [];
  if (isNum(gpu.mem_junction_c)) tsub.push("显存 " + gpu.mem_junction_c.toFixed(0) + "°C");
  if (isNum(gpu.hotspot_c)) tsub.push("热点 " + gpu.hotspot_c.toFixed(0) + "°C");
  if (isNum(gpu.temp_margin_c)) tsub.push("余量 " + gpu.temp_margin_c.toFixed(0) + "°C");
  const th = [];
  if (isNum(gpu.temp_slowdown_c)) th.push("降频" + gpu.temp_slowdown_c.toFixed(0));
  if (isNum(gpu.temp_max_c)) th.push("上限" + gpu.temp_max_c.toFixed(0));
  if (isNum(gpu.temp_shutdown_c)) th.push("关机" + gpu.temp_shutdown_c.toFixed(0));
  if (th.length) tsub.push("阈值 " + th.join("/"));
  setText("gpu-temp-sub", tsub.join("  ·  "));

  setText("gpu-vram", `${n(gpu.vram_used_mb, 0)} / ${n(gpu.vram_total_mb, 0)} MiB`);
  setBar("gpu-vram-bar", gpu.vram_usage_pct, "violet");
  const vsub = [];
  if (isNum(gpu.vram_free_mb)) vsub.push("空闲 " + gpu.vram_free_mb.toFixed(0) + " MiB");
  if (isNum(gpu.vram_reserved_mb)) vsub.push("驱动保留 " + gpu.vram_reserved_mb.toFixed(0) + " MiB");
  vsub.push("占用 " + (isNum(gpu.vram_usage_pct) ? gpu.vram_usage_pct.toFixed(1) + "%" : "n/a"));
  setText("gpu-vram-sub", vsub.join("  ·  "));

  const kv = [];
  const clk = [];
  if (isNum(gpu.core_clock_mhz)) clk.push(`${gpu.core_clock_mhz.toFixed(0)}${isNum(gpu.max_core_clock_mhz) ? "/" + gpu.max_core_clock_mhz.toFixed(0) : ""}`);
  if (isNum(gpu.mem_clock_mhz)) clk.push(`${gpu.mem_clock_mhz.toFixed(0)}${isNum(gpu.max_mem_clock_mhz) ? "/" + gpu.max_mem_clock_mhz.toFixed(0) : ""}`);
  if (clk.length) kv.push(["核心/显存时钟", clk.join("  ") + " MHz"]);
  if (isNum(gpu.power_w)) kv.push(["功耗", `${gpu.power_w.toFixed(1)} W` + (isNum(gpu.power_limit_w) ? ` / ${gpu.power_limit_w.toFixed(0)} W` : "") + (isNum(gpu.power_limit_pct) ? ` (${gpu.power_limit_pct.toFixed(0)}%)` : "")]);
  if (isNum(gpu.fan_pct) || isNum(gpu.fan_rpm)) kv.push(["风扇", `${isNum(gpu.fan_pct) ? gpu.fan_pct.toFixed(0) + "%" : "n/a"}` + (isNum(gpu.fan_rpm) ? ` · ${gpu.fan_rpm.toFixed(0)} RPM` : "") + (isNum(gpu.fan_count) ? ` · ${gpu.fan_count} 个` : "")]);
  if (isNum(gpu.encoder_pct) || isNum(gpu.decoder_pct)) kv.push(["编/解码器", `${isNum(gpu.encoder_pct) ? gpu.encoder_pct.toFixed(0) : "n/a"}% / ${isNum(gpu.decoder_pct) ? gpu.decoder_pct.toFixed(0) : "n/a"}%`]);
  if (gpu.pcie_link) kv.push(["PCIe", esc(gpu.pcie_link)]);
  if (isNum(gpu.pcie_rx_mib_s) || isNum(gpu.pcie_tx_mib_s)) kv.push(["PCIe 吞吐", `Rx ${n(gpu.pcie_rx_mib_s, 1)} / Tx ${n(gpu.pcie_tx_mib_s, 1)} MiB/s`]);
  $("gpu-kv").innerHTML = kv.map(([k, v]) => `<div><span class="k">${k}</span><span class="v">${v}</span></div>`).join("");

  const chips = [];
  if (isNum(gpu.power_limit_w)) chips.push(`<span class="chip power"><span class="k">功耗上限</span>${gpu.power_limit_w.toFixed(0)} <span class="k">W</span></span>`);
  if (isNum(gpu.fan_rpm)) chips.push(`<span class="chip"><span class="k">风扇</span>${gpu.fan_rpm.toFixed(0)} <span class="k">RPM</span></span>`);
  const tr = gpu.throttle_reasons;
  if (Array.isArray(tr)) {
    chips.push(tr.length
      ? tr.map((r) => `<span class="chip warn-source"><span class="k">限频</span>${esc(r)}</span>`).join("")
      : '<span class="chip on-source">无降频原因</span>');
  } else {
    chips.push('<span class="chip off-source">限频原因 n/a</span>');
  }
  $("gpu-chips").innerHTML = chips.join("");
}

/* --------------------------------------------------------------- 存储 */

function renderStorage(disks) {
  const list = disks || [];
  setText("storage-count", list.length ? `${list.length} 块` : "");
  $("disks").innerHTML = list.length
    ? list.map((d) => {
        const sensors = (d.temp_sensors_c || []).filter(isNum);
        const rows = [];
        if (sensors.length) rows.push(`传感器 <b>${sensors.map((t) => t.toFixed(0) + "°C").join(" / ")}</b>`);
        const lim = [];
        if (isNum(d.warning_temp_c)) lim.push(`警告 ${d.warning_temp_c.toFixed(0)}°C`);
        if (isNum(d.critical_temp_c)) lim.push(`临界 ${d.critical_temp_c.toFixed(0)}°C`);
        if (lim.length) rows.push(lim.join(" · "));
        const life = [];
        if (isNum(d.percentage_used_pct)) life.push(`磨损 <b>${d.percentage_used_pct.toFixed(0)}%</b>`);
        if (isNum(d.available_spare_pct)) life.push(`备用块 ${d.available_spare_pct.toFixed(0)}%`);
        if (isNum(d.power_on_hours)) life.push(`通电 ${d.power_on_hours.toFixed(0)} h`);
        if (life.length) rows.push(life.join(" · "));
        const io = [];
        if (isNum(d.data_written_gb)) io.push(`写 ${d.data_written_gb.toFixed(0)} GB`);
        if (isNum(d.data_read_gb)) io.push(`读 ${d.data_read_gb.toFixed(0)} GB`);
        if (io.length) rows.push(io.join(" · "));
        const act = [];
        const a = (label, v) => { if (isNum(v)) act.push(`${label} ${v.toFixed(0)}%`); };
        a("读", d.activity_read_pct); a("写", d.activity_write_pct); a("总", d.activity_total_pct);
        const tp = [];
        if (isNum(d.read_mib_s)) tp.push(`${d.read_mib_s.toFixed(1)}`);
        if (isNum(d.write_mib_s)) tp.push(`${d.write_mib_s.toFixed(1)}`);
        if (act.length || tp.length) {
          rows.push(`${act.length ? "活动 <b>" + act.join(" ") + "</b>" : ""}${tp.length ? "  吞吐 R/W <b>" + tp.join(" / ") + " MiB/s</b>" : ""}`);
        }
        return `
        <div class="disk">
          <div class="title"><span class="idx">#${d.index}</span>${esc(d.name || "")}</div>
          <div class="temp">${isNum(d.temp_c) ? d.temp_c.toFixed(1) + "°C" : '<span class="na">n/a</span>'}
            <span class="gauge-sub">${esc(d.bus || "")} · ${esc(d.source || "")}</span></div>
          ${rows.map((r) => `<div class="row">${r}</div>`).join("")}
        </div>`;
      }).join("")
    : '<div class="gauge-sub">没有读到存储设备</div>';
}

/* --------------------------------------------------------------- 主板 */

const KIND_TITLE = { fan: "风扇", temperature: "温度", voltage: "电压" };

function renderBoard(superio) {
  const has = !!superio;
  $("board-none").classList.toggle("hidden", has);
  setText("board-chip", has ? `${esc(superio.chip)} · ${esc(superio.profile)}` : "");
  if (!has) { $("board-sensors").innerHTML = ""; return; }

  const groups = {};
  for (const s of superio.sensors || []) (groups[s.kind] ||= []).push(s);
  $("board-sensors").innerHTML = Object.keys(groups).map((kind) => `
    <div class="group">
      <div class="t">${KIND_TITLE[kind] || esc(kind)}</div>
      ${groups[kind].map((s) => `<div class="item"><span class="n">${esc(s.name)}</span><span>${isNum(s.value) ? s.value.toFixed(s.unit === "V" ? 2 : 0) : "n/a"} ${esc(s.unit)}</span></div>`).join("")}
    </div>`).join("");
}

/* ------------------------------------------------------------ 数据源 */

const SOURCE_LABELS = {
  pawnio: "PawnIO 驱动",
  pawnio_access_denied: "PawnIO 被拒",
  nvml: "NVML",
  nvidia_smi: "nvidia-smi",
  wmi: "WMI 温度兜底",
  storage_smart: "NVMe SMART",
  storage_perf: "磁盘活动率",
  smu: "SMU PM 表",
  superio: "主板 SuperIO",
  spd: "DDR5 SPD",
  msr_cores: "每核 MSR",
};

function renderSources(sources, info) {
  const items = Object.entries(sources || {});
  $("sources").innerHTML = items.map(([k, v]) => {
    const label = SOURCE_LABELS[k] || k;
    const cls = k === "pawnio_access_denied"
      ? (v ? "warn-source" : "off-source")
      : (v ? "on-source" : "off-source");
    const text = k === "pawnio_access_denied" ? (v ? "被拒（非提权）" : "未发生") : (v ? "可用" : "不可用");
    return `<span class="chip ${cls}"><span class="k">${esc(label)}</span>${text}</span>`;
  }).join("");

  const notes = [];
  if (sources && sources.pawnio_access_denied && !sources.pawnio) {
    notes.push("CPU 温度/功耗、每核 MSR、SMU 电压、主板 SuperIO、DDR5 SPD 都需要管理员权限 —— 用管理员身份启动这个服务即可全部解锁。");
  }
  if (info && info.smu_pm_table_version) {
    notes.push(`SMU PM 表版本 0x${info.smu_pm_table_version.toString(16).padStart(8, "0")}。`);
  }
  $("sources-note").textContent = notes.join(" ");

  const banner = $("banner");
  if (sources && sources.pawnio_access_denied && !sources.pawnio) {
    banner.innerHTML = "⚠ 当前为非提权运行：温度/电压/每核数据大面积缺失。<br>以管理员身份重跑 <code>python web/server.py</code> 可解锁全部数据源。";
    banner.classList.remove("hidden");
  } else {
    banner.classList.add("hidden");
  }
}

/* ------------------------------------------------------------- 运行时 */

function renderMetrics(m) {
  if (!m) return;
  renderCpu(m.cpu || {});
  renderMemory(m.memory || {});
  renderDimms(m.dimms);
  renderGpu(m.gpu);
  renderStorage(m.storage);
  renderBoard(m.superio);
  const ts = m.ts_ms ? new Date(m.ts_ms).toLocaleTimeString("zh-CN", { hour12: false }) : "—";
  setText("ts", ts);
}

function renderInfo(info) {
  if (!info) return;
  setText("machine", `${esc(info.cpu_name)} · ${info.logical_cores} 线程 · family 0x${(info.cpu_family || 0).toString(16).toUpperCase()} model 0x${(info.cpu_model || 0).toString(16).toUpperCase()} · ${n(info.total_memory_gb, 1)} GB RAM${info.gpu_name ? " · " + esc(info.gpu_name) : ""}`);
  setText("version", esc(info.version || ""));
  setText("foot-ver", esc(info.version || ""));
  renderSources(info.sources, info);
}

let ws = null;
let retryMs = 500;
let wantInterval = parseFloat($("interval").value) || 0.5;

function setConn(on, text) {
  const dot = $("conn");
  dot.className = "dot " + (on ? "on" : "off");
  setText("conn-text", text);
}

function connect() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  ws = new WebSocket(`${proto}://${location.host}/ws?interval=${wantInterval}`);

  ws.onopen = () => { retryMs = 500; setConn(true, "已连接"); };
  ws.onclose = () => {
    setConn(false, `断开，${(retryMs / 1000).toFixed(1)}s 后重连`);
    setTimeout(connect, retryMs);
    retryMs = Math.min(retryMs * 2, 5000);
  };
  ws.onerror = () => { try { ws.close(); } catch (_) {} };
  ws.onmessage = (ev) => {
    let msg;
    try { msg = JSON.parse(ev.data); } catch (_) { return; }
    if (msg.type === "hello") {
      if (msg.info) renderInfo(msg.info);
      setText("foot-dll", `DLL: ${esc(msg.dll || "")}`);
      setText("version", esc(msg.version || ""));
    } else if (msg.type === "metrics") {
      renderMetrics(msg.data);
    } else if (msg.type === "info") {
      renderInfo(msg.data);
    }
  };
}

$("interval").addEventListener("change", (e) => {
  wantInterval = parseFloat(e.target.value) || 0.5;
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify({ cmd: "interval", value: wantInterval }));
  }
});

// 先把 info 抓一遍，页面不用等 WebSocket 建连
fetch("/api/info").then((r) => r.ok ? r.json() : null).then(renderInfo).catch(() => {});
fetch("/api/health").then((r) => r.ok ? r.json() : null)
  .then((h) => { if (h) setText("foot-dll", `DLL: ${esc(h.dll || "")}`); })
  .catch(() => {});

connect();
