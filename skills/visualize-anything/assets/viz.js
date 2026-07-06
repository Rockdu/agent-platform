/* viz.js — dependency-free, self-contained visualization primitives.
 * No CDN, no external fetch: safe to inline into a single HTML file and
 * to publish as a Claude Artifact (strict CSP). Everything renders as
 * SVG / HTML tables so the output stays visual-first.
 *
 * Public API (window.VIZ):
 *   VIZ.kpis(el, tiles)              KPI/stat tile row
 *   VIZ.table(el, cols, rows, opts)  sortable table with typed cells
 *   VIZ.bars(el, data, opts)         horizontal or vertical bar chart
 *   VIZ.donut(el, segments, opts)    donut / distribution
 *   VIZ.dag(el, nodes, edges, opts)  layered left→right DAG (acyclic flows)
 *   VIZ.graph(el, nodes, edges, opts)radial relationship graph (may cycle)
 *   VIZ.timeline(el, tracks, opts)   gantt-style timeline
 *   VIZ.heat(el, matrix, opts)       heatmap grid
 *
 * `el` may be an element or a selector string. Every renderer is pure:
 * call it again with new data to redraw. Colors come from CSS custom
 * properties so light/dark themes and the palette live in viz.css.
 */
(function () {
  "use strict";

  var NS = "http://www.w3.org/2000/svg";
  var STATUS = ["ok", "warn", "err", "busy", "idle", "info", "muted"];

  function el(sel) {
    return typeof sel === "string" ? document.querySelector(sel) : sel;
  }
  function esc(s) {
    return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
    });
  }
  function svg(tag, attrs, kids) {
    var n = document.createElementNS(NS, tag);
    if (attrs) for (var k in attrs) if (attrs[k] != null) n.setAttribute(k, attrs[k]);
    (kids || []).forEach(function (c) { n.appendChild(c); });
    return n;
  }
  function css(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }
  // Map an arbitrary status token to one of the palette slots.
  function statusVar(s) {
    s = String(s || "").toLowerCase();
    var map = {
      ready: "ok", ok: "ok", success: "ok", done: "ok", passed: "ok", merged: "ok", open: "ok",
      warn: "warn", warning: "warn", backingoff: "warn", pending: "warn", review: "warn", degraded: "warn",
      err: "err", error: "err", exited: "err", failed: "err", unrecoverable: "err", closed: "err", transportcorrupt: "err",
      busy: "busy", running: "busy", spawning: "busy", active: "busy", "in_progress": "busy",
      idle: "idle", waiting: "idle", queued: "idle",
      info: "info", shuttingdown: "info",
    };
    return "--st-" + (map[s] || "muted");
  }
  function statusColor(s) { return "var(" + statusVar(s) + ")"; }
  // Deterministic categorical color from the palette ring.
  function catColor(i) { return "var(--cat-" + (Math.abs(i | 0) % 8) + ")"; }

  function clear(node) { while (node.firstChild) node.removeChild(node.firstChild); }
  function box(title, subtitle) {
    var w = document.createElement("figure");
    w.className = "viz-fig";
    if (title) {
      var cap = document.createElement("figcaption");
      cap.className = "viz-cap";
      cap.innerHTML = "<span>" + esc(title) + "</span>" +
        (subtitle ? "<small>" + esc(subtitle) + "</small>" : "");
      w.appendChild(cap);
    }
    return w;
  }

  // ---- KPI tiles -----------------------------------------------------
  function kpis(target, tiles, opts) {
    var host = el(target); clear(host);
    var grid = document.createElement("div");
    grid.className = "viz-kpis";
    (tiles || []).forEach(function (t) {
      var d = document.createElement("div");
      d.className = "viz-kpi";
      if (t.status) d.style.setProperty("--accent", statusColor(t.status));
      var delta = "";
      if (t.delta != null && t.delta !== "") {
        var up = Number(t.delta) > 0, flat = Number(t.delta) === 0;
        delta = '<span class="viz-delta ' + (flat ? "flat" : up ? "up" : "down") + '">' +
          (flat ? "→" : up ? "▲" : "▼") + " " + esc(t.delta) + "</span>";
      }
      d.innerHTML =
        '<div class="viz-kpi-label">' + esc(t.label) + "</div>" +
        '<div class="viz-kpi-value">' + esc(t.value) + (t.unit ? '<small>' + esc(t.unit) + "</small>" : "") + "</div>" +
        '<div class="viz-kpi-foot">' + delta + (t.note ? '<span class="viz-kpi-note">' + esc(t.note) + "</span>" : "") + "</div>";
      if (Array.isArray(t.spark) && t.spark.length > 1) d.appendChild(spark(t.spark, d.style.getPropertyValue("--accent") || "var(--cat-0)"));
      grid.appendChild(d);
    });
    host.appendChild(grid);
    return host;
  }

  // Sparkline (Tufte: word-sized, axis-free, one end dot). data: number[].
  function spark(data, color) {
    var w = 108, h = 26, pad = 2;
    var mn = Math.min.apply(null, data), mx = Math.max.apply(null, data), rng = (mx - mn) || 1;
    var pts = data.map(function (v, i) {
      return [pad + i / (data.length - 1) * (w - pad * 2), h - pad - (v - mn) / rng * (h - pad * 2)];
    });
    var s = svg("svg", { viewBox: "0 0 " + w + " " + h, class: "viz-spark", width: w, height: h, preserveAspectRatio: "none" });
    s.appendChild(svg("polyline", { points: pts.map(function (p) { return p[0].toFixed(1) + "," + p[1].toFixed(1); }).join(" "), fill: "none", stroke: color, "stroke-width": 1.5 }));
    var last = pts[pts.length - 1];
    s.appendChild(svg("circle", { cx: last[0], cy: last[1], r: 2.2, fill: color }));
    return s;
  }

  // ---- Table ---------------------------------------------------------
  // cols: [{key, label, type?: 'text'|'num'|'status'|'bar'|'code'|'chips', max?, align?}]
  function table(target, cols, rows, opts) {
    opts = opts || {};
    var host = el(target); clear(host);
    var wrap = box(opts.title, opts.subtitle);
    var scroller = document.createElement("div");
    scroller.className = "viz-tablewrap";
    var t = document.createElement("table");
    t.className = "viz-table";
    var thead = document.createElement("thead");
    var htr = document.createElement("tr");
    cols.forEach(function (c, ci) {
      var th = document.createElement("th");
      th.textContent = c.label != null ? c.label : c.key;
      if (c.align) th.style.textAlign = c.align;
      if (opts.sortable !== false) {
        th.className = "viz-sortable";
        th.addEventListener("click", function () { sortBy(ci); });
      }
      htr.appendChild(th);
    });
    thead.appendChild(htr); t.appendChild(thead);
    var tbody = document.createElement("tbody");
    var max = {};
    cols.forEach(function (c) {
      if (c.type === "bar") max[c.key] = c.max != null ? c.max : Math.max.apply(null, rows.map(function (r) { return Number(r[c.key]) || 0; }).concat([1]));
    });
    var sortState = { ci: opts.sortCol != null ? opts.sortCol : -1, dir: opts.sortDir === "desc" ? -1 : 1 };
    function draw(data) {
      clear(tbody);
      data.forEach(function (r) {
        var tr = document.createElement("tr");
        cols.forEach(function (c) {
          var td = document.createElement("td");
          var v = r[c.key];
          if (c.align) td.style.textAlign = c.align;
          switch (c.type) {
            case "status":
              td.innerHTML = '<span class="viz-badge" style="--b:' + statusColor(v) + '">' + esc(v) + "</span>";
              break;
            case "bar":
              var pct = Math.max(0, Math.min(100, (Number(v) || 0) / max[c.key] * 100));
              td.innerHTML = '<span class="viz-cellbar"><span style="width:' + pct.toFixed(1) + "%;background:" +
                (c.color ? c.color : catColor(0)) + '"></span></span><small class="viz-cellbar-n">' + esc(v) + "</small>";
              break;
            case "chips":
              td.innerHTML = (Array.isArray(v) ? v : [v]).filter(Boolean).map(function (x, i) {
                return '<span class="viz-chip" style="--b:' + catColor(i) + '">' + esc(x) + "</span>";
              }).join(" ");
              break;
            case "code":
              td.innerHTML = "<code>" + esc(v) + "</code>";
              break;
            case "num":
              td.className = "viz-num"; td.textContent = v == null ? "" : v;
              break;
            default:
              td.textContent = v == null ? "" : v;
          }
          tr.appendChild(td);
        });
        tbody.appendChild(tr);
      });
    }
    function sortBy(ci) {
      sortState.dir = sortState.ci === ci ? -sortState.dir : 1;
      sortState.ci = ci;
      var key = cols[ci].key, num = cols[ci].type === "num" || cols[ci].type === "bar";
      var sorted = rows.slice().sort(function (a, b) {
        var x = a[key], y = b[key];
        if (num) return (Number(x) - Number(y)) * sortState.dir;
        return String(x).localeCompare(String(y)) * sortState.dir;
      });
      draw(sorted);
    }
    t.appendChild(tbody);
    if (sortState.ci >= 0) sortBy(sortState.ci); else draw(rows);
    scroller.appendChild(t); wrap.appendChild(scroller); host.appendChild(wrap);
    return host;
  }

  // ---- Bar chart -----------------------------------------------------
  // data: [{label, value, status?, color?}]
  function bars(target, data, opts) {
    opts = opts || {};
    var host = el(target); clear(host);
    var wrap = box(opts.title, opts.subtitle);
    var horiz = opts.orient !== "vertical";
    var maxV = opts.max != null ? opts.max : Math.max.apply(null, data.map(function (d) { return d.value; }).concat([1]));
    var pad = 8, rowH = 26, gap = 8, labelW = horiz ? (opts.labelWidth || 120) : 0;
    var W = opts.width || 640;
    if (horiz) {
      var H = pad * 2 + data.length * (rowH + gap);
      var s = svg("svg", { viewBox: "0 0 " + W + " " + H, class: "viz-svg", width: "100%", height: H });
      data.forEach(function (d, i) {
        var y = pad + i * (rowH + gap);
        var bw = (W - labelW - pad - 46) * (Number(d.value) || 0) / maxV;
        s.appendChild(svg("text", { x: 0, y: y + rowH / 2 + 4, class: "viz-t-label" }, [txt(d.label)]));
        s.appendChild(svg("rect", { x: labelW, y: y, width: Math.max(1, bw), height: rowH, rx: 5, fill: d.color || (d.status ? statusColor(d.status) : catColor(i)) }));
        s.appendChild(svg("text", { x: labelW + bw + 6, y: y + rowH / 2 + 4, class: "viz-t-val" }, [txt(d.value)]));
      });
      wrap.appendChild(s);
    } else {
      var H2 = opts.height || 240, barW = (W - pad * 2) / data.length * 0.7, step = (W - pad * 2) / data.length;
      var s2 = svg("svg", { viewBox: "0 0 " + W + " " + H2, class: "viz-svg", width: "100%", height: H2 });
      data.forEach(function (d, i) {
        var bh = (H2 - 34) * (Number(d.value) || 0) / maxV, x = pad + i * step + (step - barW) / 2;
        s2.appendChild(svg("rect", { x: x, y: H2 - 22 - bh, width: barW, height: Math.max(1, bh), rx: 4, fill: d.color || (d.status ? statusColor(d.status) : catColor(i)) }));
        s2.appendChild(svg("text", { x: x + barW / 2, y: H2 - 8, class: "viz-t-label", "text-anchor": "middle" }, [txt(d.label)]));
        s2.appendChild(svg("text", { x: x + barW / 2, y: H2 - 26 - bh, class: "viz-t-val", "text-anchor": "middle" }, [txt(d.value)]));
      });
      wrap.appendChild(s2);
    }
    host.appendChild(wrap);
    return host;
  }
  function txt(s) { return document.createTextNode(String(s == null ? "" : s)); }

  // ---- Donut ---------------------------------------------------------
  // segments: [{label, value, status?, color?}]
  function donut(target, segments, opts) {
    opts = opts || {};
    var host = el(target); clear(host);
    var wrap = box(opts.title, opts.subtitle);
    var R = 70, r = 44, cx = 90, cy = 90, total = segments.reduce(function (a, s) { return a + (Number(s.value) || 0); }, 0) || 1;
    var s = svg("svg", { viewBox: "0 0 320 180", class: "viz-svg", width: "100%", height: 180 });
    var a0 = -Math.PI / 2;
    segments.forEach(function (seg, i) {
      var frac = (Number(seg.value) || 0) / total, a1 = a0 + frac * Math.PI * 2;
      var large = frac > 0.5 ? 1 : 0;
      var p = [
        "M", cx + R * Math.cos(a0), cy + R * Math.sin(a0),
        "A", R, R, 0, large, 1, cx + R * Math.cos(a1), cy + R * Math.sin(a1),
        "L", cx + r * Math.cos(a1), cy + r * Math.sin(a1),
        "A", r, r, 0, large, 0, cx + r * Math.cos(a0), cy + r * Math.sin(a0), "Z"
      ].join(" ");
      s.appendChild(svg("path", { d: p, fill: seg.color || (seg.status ? statusColor(seg.status) : catColor(i)) }));
      a0 = a1;
    });
    if (opts.center) {
      s.appendChild(svg("text", { x: cx, y: cy - 2, class: "viz-donut-c", "text-anchor": "middle" }, [txt(opts.center)]));
      if (opts.centerSub) s.appendChild(svg("text", { x: cx, y: cy + 16, class: "viz-donut-cs", "text-anchor": "middle" }, [txt(opts.centerSub)]));
    }
    segments.forEach(function (seg, i) {
      var y = 34 + i * 22;
      s.appendChild(svg("rect", { x: 190, y: y - 10, width: 12, height: 12, rx: 3, fill: seg.color || (seg.status ? statusColor(seg.status) : catColor(i)) }));
      s.appendChild(svg("text", { x: 208, y: y, class: "viz-legend" }, [txt(seg.label + "  " + seg.value)]));
    });
    wrap.appendChild(s); host.appendChild(wrap);
    return host;
  }

  // ---- Layered DAG (acyclic flow, left → right) ----------------------
  // nodes: [{id, label, group?, status?, sub?}]  edges: [{from, to, label?}]
  function dag(target, nodes, edges, opts) {
    opts = opts || {};
    var host = el(target); clear(host);
    var wrap = box(opts.title, opts.subtitle);
    var byId = {}; nodes.forEach(function (n) { byId[n.id] = n; });
    // longest-path layering
    var succ = {}, indeg = {};
    nodes.forEach(function (n) { succ[n.id] = []; indeg[n.id] = 0; });
    edges.forEach(function (e) { if (byId[e.from] && byId[e.to]) { succ[e.from].push(e.to); indeg[e.to]++; } });
    var layer = {}, q = nodes.filter(function (n) { return indeg[n.id] === 0; }).map(function (n) { return n.id; });
    q.forEach(function (id) { layer[id] = 0; });
    var deg = {}; nodes.forEach(function (n) { deg[n.id] = indeg[n.id]; });
    while (q.length) {
      var id = q.shift();
      succ[id].forEach(function (v) {
        layer[v] = Math.max(layer[v] || 0, (layer[id] || 0) + 1);
        if (--deg[v] === 0) q.push(v);
      });
    }
    nodes.forEach(function (n) { if (layer[n.id] == null) layer[n.id] = 0; });
    var cols = {};
    nodes.forEach(function (n) { (cols[layer[n.id]] = cols[layer[n.id]] || []).push(n); });
    var nCols = Math.max.apply(null, Object.keys(cols).map(Number)).valueOf() + 1;
    var colW = opts.colW || 190, nodeH = 46, vGap = 20, pad = 16;
    var maxRows = Math.max.apply(null, Object.keys(cols).map(function (k) { return cols[k].length; }));
    var W = pad * 2 + nCols * colW, H = pad * 2 + maxRows * (nodeH + vGap);
    var pos = {};
    Object.keys(cols).forEach(function (L) {
      cols[L].forEach(function (n, i) {
        var colCount = cols[L].length;
        var yOff = (maxRows - colCount) * (nodeH + vGap) / 2;
        pos[n.id] = { x: pad + Number(L) * colW, y: pad + yOff + i * (nodeH + vGap) };
      });
    });
    var s = svg("svg", { viewBox: "0 0 " + W + " " + H, class: "viz-svg viz-dag", width: "100%", height: Math.min(H, opts.maxHeight || 9999) });
    s.appendChild(defsArrow());
    edges.forEach(function (e) {
      var a = pos[e.from], b = pos[e.to]; if (!a || !b) return;
      var x1 = a.x + colW - 40, y1 = a.y + nodeH / 2, x2 = b.x, y2 = b.y + nodeH / 2;
      var mx = (x1 + x2) / 2;
      s.appendChild(svg("path", { d: "M" + x1 + "," + y1 + " C" + mx + "," + y1 + " " + mx + "," + y2 + " " + x2 + "," + y2, class: "viz-edge", "marker-end": "url(#viz-arrow)" }));
      if (e.label) s.appendChild(svg("text", { x: mx, y: (y1 + y2) / 2 - 4, class: "viz-edge-label", "text-anchor": "middle" }, [txt(e.label)]));
    });
    nodes.forEach(function (n) {
      var p = pos[n.id], nw = colW - 46;
      var g = svg("g", { transform: "translate(" + p.x + "," + p.y + ")" });
      g.appendChild(svg("rect", { width: nw, height: nodeH, rx: 8, class: "viz-node", style: "--accent:" + (n.status ? statusColor(n.status) : catColor(n.group ? hash(n.group) : 0)) }));
      g.appendChild(svg("rect", { width: 5, height: nodeH, rx: 2, fill: n.status ? statusColor(n.status) : catColor(n.group ? hash(n.group) : 0) }));
      g.appendChild(svg("text", { x: 12, y: n.sub ? 20 : 27, class: "viz-node-label" }, [txt(clip(n.label, 22))]));
      if (n.sub) g.appendChild(svg("text", { x: 12, y: 36, class: "viz-node-sub" }, [txt(clip(n.sub, 26))]));
      s.appendChild(g);
    });
    wrap.appendChild(s); host.appendChild(wrap);
    return host;
  }
  function defsArrow() {
    var m = svg("marker", { id: "viz-arrow", viewBox: "0 0 10 10", refX: 9, refY: 5, markerWidth: 7, markerHeight: 7, orient: "auto-start-reverse" });
    m.appendChild(svg("path", { d: "M0,0 L10,5 L0,10 z", class: "viz-arrowhead" }));
    var d = svg("defs"); d.appendChild(m); return d;
  }
  function clip(s, n) { s = String(s == null ? "" : s); return s.length > n ? s.slice(0, n - 1) + "…" : s; }
  function hash(s) { var h = 0; s = String(s); for (var i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0; return h; }

  // ---- Radial relationship graph (may contain cycles) ---------------
  function graph(target, nodes, edges, opts) {
    opts = opts || {};
    var host = el(target); clear(host);
    var wrap = box(opts.title, opts.subtitle);
    var W = opts.width || 640, H = opts.height || 420, cx = W / 2, cy = H / 2;
    var hub = opts.hub && nodes.some(function (n) { return n.id === opts.hub; });
    var ring = nodes.filter(function (n) { return n.id !== opts.hub; });
    var R = Math.min(W, H) / 2 - 70, pos = {};
    if (hub) pos[opts.hub] = { x: cx, y: cy };
    ring.forEach(function (n, i) {
      var a = (i / ring.length) * Math.PI * 2 - Math.PI / 2;
      pos[n.id] = { x: cx + R * Math.cos(a), y: cy + R * Math.sin(a) };
    });
    var byId = {}; nodes.forEach(function (n) { byId[n.id] = n; });
    var s = svg("svg", { viewBox: "0 0 " + W + " " + H, class: "viz-svg", width: "100%", height: H });
    s.appendChild(defsArrow());
    edges.forEach(function (e) {
      var a = pos[e.from], b = pos[e.to]; if (!a || !b) return;
      s.appendChild(svg("line", { x1: a.x, y1: a.y, x2: b.x, y2: b.y, class: "viz-edge", "marker-end": "url(#viz-arrow)" }));
    });
    nodes.forEach(function (n) {
      var p = pos[n.id]; if (!p) return;
      var rad = n.id === opts.hub ? 30 : 22;
      var g = svg("g", { transform: "translate(" + p.x + "," + p.y + ")" });
      g.appendChild(svg("circle", { r: rad, class: "viz-gnode", style: "--accent:" + (n.status ? statusColor(n.status) : catColor(n.group ? hash(n.group) : 0)) }));
      g.appendChild(svg("text", { y: rad + 14, class: "viz-gnode-label", "text-anchor": "middle" }, [txt(clip(n.label, 16))]));
      if (n.count != null) g.appendChild(svg("text", { y: 5, class: "viz-gnode-n", "text-anchor": "middle" }, [txt(n.count)]));
      s.appendChild(g);
    });
    wrap.appendChild(s); host.appendChild(wrap);
    return host;
  }

  // ---- Timeline / gantt ---------------------------------------------
  // tracks: [{label, bars:[{start,end,label?,status?}]}]  times are numbers (ms/epoch) or same-unit
  function timeline(target, tracks, opts) {
    opts = opts || {};
    var host = el(target); clear(host);
    var wrap = box(opts.title, opts.subtitle);
    var all = [];
    tracks.forEach(function (t) { (t.bars || []).forEach(function (b) { all.push(b.start); all.push(b.end != null ? b.end : b.start); }); });
    var min = opts.min != null ? opts.min : Math.min.apply(null, all), max = opts.max != null ? opts.max : Math.max.apply(null, all);
    if (!isFinite(min)) { min = 0; max = 1; }
    if (max === min) max = min + 1;
    var labelW = opts.labelWidth || 150, pad = 10, rowH = 24, gap = 6, W = opts.width || 720;
    var H = pad * 2 + tracks.length * (rowH + gap) + 18;
    var plotW = W - labelW - pad;
    function X(t) { return labelW + (t - min) / (max - min) * plotW; }
    var s = svg("svg", { viewBox: "0 0 " + W + " " + H, class: "viz-svg", width: "100%", height: H });
    // gridlines
    var ticks = opts.ticks || 4;
    for (var i = 0; i <= ticks; i++) {
      var tv = min + (max - min) * i / ticks, x = X(tv);
      s.appendChild(svg("line", { x1: x, y1: pad, x2: x, y2: H - 16, class: "viz-grid" }));
      s.appendChild(svg("text", { x: x, y: H - 4, class: "viz-tick", "text-anchor": "middle" }, [txt(opts.fmt ? opts.fmt(tv) : Math.round(tv))]));
    }
    tracks.forEach(function (t, i) {
      var y = pad + i * (rowH + gap);
      s.appendChild(svg("text", { x: 0, y: y + rowH / 2 + 4, class: "viz-t-label" }, [txt(clip(t.label, 20))]));
      (t.bars || []).forEach(function (b) {
        var x1 = X(b.start), x2 = X(b.end != null ? b.end : b.start);
        var w = Math.max(3, x2 - x1);
        var g = svg("g");
        var rect = svg("rect", { x: x1, y: y, width: w, height: rowH, rx: 4, fill: b.status ? statusColor(b.status) : catColor(i) });
        var tip = svg("title"); tip.textContent = (b.label || t.label) + (b.end != null ? "  (" + (b.end - b.start) + ")" : "");
        rect.appendChild(tip); g.appendChild(rect);
        if (b.label && w > 44) g.appendChild(svg("text", { x: x1 + 5, y: y + rowH / 2 + 4, class: "viz-bar-label" }, [txt(clip(b.label, Math.floor(w / 7)))]));
        s.appendChild(g);
      });
    });
    wrap.appendChild(s); host.appendChild(wrap);
    return host;
  }

  // ---- Heatmap -------------------------------------------------------
  // matrix: {rows:[labels], cols:[labels], values:[[..]], max?}
  function heat(target, matrix, opts) {
    opts = opts || {};
    var host = el(target); clear(host);
    var wrap = box(opts.title, opts.subtitle);
    var rows = matrix.rows, colsL = matrix.cols, V = matrix.values;
    var max = matrix.max != null ? matrix.max : Math.max.apply(null, V.map(function (r) { return Math.max.apply(null, r); }).concat([1]));
    var cell = opts.cell || 30, labelW = opts.labelWidth || 90, topH = 60, pad = 6;
    var W = labelW + colsL.length * cell + pad, H = topH + rows.length * cell + pad;
    var s = svg("svg", { viewBox: "0 0 " + W + " " + H, class: "viz-svg", width: "100%", height: H });
    colsL.forEach(function (c, j) {
      s.appendChild(svg("text", { x: labelW + j * cell + cell / 2, y: topH - 6, class: "viz-heat-col", transform: "rotate(-40 " + (labelW + j * cell + cell / 2) + " " + (topH - 6) + ")" }, [txt(clip(c, 12))]));
    });
    rows.forEach(function (rlab, i) {
      s.appendChild(svg("text", { x: labelW - 6, y: topH + i * cell + cell / 2 + 4, class: "viz-t-label", "text-anchor": "end" }, [txt(clip(rlab, 14))]));
      colsL.forEach(function (c, j) {
        var v = (V[i] && V[i][j]) || 0, t = v / max;
        var g = svg("g");
        var rect = svg("rect", { x: labelW + j * cell + 1, y: topH + i * cell + 1, width: cell - 2, height: cell - 2, rx: 3, fill: "var(--cat-0)", "fill-opacity": (0.12 + 0.88 * t).toFixed(3) });
        var tip = svg("title"); tip.textContent = rlab + " × " + c + " = " + v; rect.appendChild(tip);
        g.appendChild(rect);
        if (opts.showValues && v) g.appendChild(svg("text", { x: labelW + j * cell + cell / 2, y: topH + i * cell + cell / 2 + 4, class: "viz-heat-v", "text-anchor": "middle" }, [txt(v)]));
        s.appendChild(g);
      });
    });
    wrap.appendChild(s); host.appendChild(wrap);
    return host;
  }

  // ---- Collapsible span/chain tree (native <details>, zero-JS toggle) --
  // nodes: [{label, sub?, status?, badges?:[{text,status?}], meta?, children?}]
  // opts.open: expand depth N by default (default 1)
  function tree(target, nodes, opts) {
    opts = opts || {};
    var host = el(target); clear(host);
    var wrap = box(opts.title, opts.subtitle);
    var root = document.createElement("div");
    root.className = "viz-tree";
    function render(list, depth, parent) {
      list.forEach(function (n) {
        var kids = n.children && n.children.length;
        var node = document.createElement(kids ? "details" : "div");
        node.className = "viz-tnode" + (kids ? "" : " leaf");
        node.style.setProperty("--depth", depth);
        if (kids && depth < (opts.open != null ? opts.open : 1)) node.open = true;
        var head = document.createElement(kids ? "summary" : "div");
        head.className = "viz-trow";
        head.style.setProperty("--accent", n.status ? statusColor(n.status) : catColor(depth));
        var badges = (n.badges || []).map(function (b) {
          return '<span class="viz-badge" style="--b:' + statusColor(b.status || "muted") + '">' + esc(b.text) + "</span>";
        }).join("");
        head.innerHTML =
          '<span class="viz-tbar"></span>' +
          '<span class="viz-tlabel">' + esc(n.label) + "</span>" +
          (n.sub ? '<code class="viz-tsub">' + esc(n.sub) + "</code>" : "") +
          '<span class="viz-tbadges">' + badges + "</span>" +
          (n.meta ? '<span class="viz-tmeta">' + esc(n.meta) + "</span>" : "");
        node.appendChild(head);
        parent.appendChild(node);
        if (kids) render(n.children, depth + 1, node);
      });
    }
    render(nodes, 0, root);
    wrap.appendChild(root); host.appendChild(wrap);
    return host;
  }

  // ---- Agent map: labeled relationships + expandable "doing" cards -----
  // The primary view for "how agents relate + what each is doing".
  // data: { nodes:[{id,label,kind,status,doing,topic?,trace?}], edges:[{from,to,label,status?}] }
  //   node.doing  = ONE incisive line: what this agent is doing right now.
  //   node.topic  = optional muted subline (overall subject).
  //   node.trace  = { kpis:[{label,value}], tools:[{label,value,status}], steps:[{label,sub,status,meta}] }
  //   edge.label  = the relationship type (drawn on the connector).
  // Layered top→down; connectors are measured from the live DOM so they follow
  // reflow when a card expands. Fully self-contained, no external deps.
  function agentmap(target, data, opts) {
    opts = opts || {};
    var host = el(target); clear(host);
    var wrap = box(opts.title, opts.subtitle);
    var nodes = data.nodes || [], edges = data.edges || [];
    var byId = {}; nodes.forEach(function (n) { byId[n.id] = n; });
    // longest-path layering (roots have indegree 0)
    var succ = {}, indeg = {};
    nodes.forEach(function (n) { succ[n.id] = []; indeg[n.id] = 0; });
    edges.forEach(function (e) { if (byId[e.from] && byId[e.to]) { succ[e.from].push(e.to); indeg[e.to]++; } });
    var layer = {}, q = nodes.filter(function (n) { return indeg[n.id] === 0; }).map(function (n) { return n.id; });
    q.forEach(function (id) { layer[id] = 0; });
    var deg = {}; nodes.forEach(function (n) { deg[n.id] = indeg[n.id]; });
    while (q.length) {
      var id = q.shift();
      succ[id].forEach(function (v) { layer[v] = Math.max(layer[v] || 0, (layer[id] || 0) + 1); if (--deg[v] === 0) q.push(v); });
    }
    nodes.forEach(function (n) { if (layer[n.id] == null) layer[n.id] = 0; });

    var map = document.createElement("div"); map.className = "viz-amap";
    var svgLayer = svg("svg", { class: "viz-amap-edges", "aria-hidden": "true" });
    map.appendChild(svgLayer);
    var maxLayer = Math.max.apply(null, nodes.map(function (n) { return layer[n.id]; }).concat([0]));
    var rows = {};
    for (var L = 0; L <= maxLayer; L++) { var r = document.createElement("div"); r.className = "viz-amap-row"; rows[L] = r; map.appendChild(r); }
    var cardById = {};
    nodes.forEach(function (n) {
      var d = document.createElement("details"); d.className = "viz-agent"; d.setAttribute("data-id", n.id);
      var accent = n.status ? statusColor(n.status) : catColor(hash(n.kind || ""));
      d.style.setProperty("--accent", accent);
      if (n.trace && opts.open) d.open = true;
      var sum = document.createElement("summary"); sum.className = "viz-agent-head";
      var kindChip = n.kind ? '<span class="viz-agent-kind">' + esc(n.kind) + "</span>" : "";
      var statusBadge = n.status ? '<span class="viz-badge" style="--b:' + accent + '">' + esc(n.status) + "</span>" : "";
      var exp = n.trace ? '<span class="viz-agent-exp">details</span>' : "";
      sum.innerHTML =
        '<div class="viz-agent-top"><span class="viz-agent-name">' + esc(n.label) + "</span>" + kindChip + statusBadge + exp + "</div>" +
        '<div class="viz-agent-doing">' + esc(n.doing || "—") + "</div>" +
        (n.topic ? '<div class="viz-agent-topic">' + esc(n.topic) + "</div>" : "");
      d.appendChild(sum);
      if (n.trace) { var body = document.createElement("div"); body.className = "viz-agent-body"; body.appendChild(renderTrace(n.trace)); d.appendChild(body); }
      rows[layer[n.id]].appendChild(d);
      cardById[n.id] = d;
    });
    wrap.appendChild(map); host.appendChild(wrap);

    function renderTrace(tr) {
      var f = document.createElement("div"); f.className = "viz-agent-trace";
      if (tr.summary) { var sm = document.createElement("div"); sm.className = "viz-agent-summary"; sm.textContent = tr.summary; f.appendChild(sm); }
      if (tr.kpis && tr.kpis.length) {
        var k = document.createElement("div"); k.className = "viz-agent-kpis";
        k.innerHTML = tr.kpis.map(function (x) { return '<span class="viz-ministat"><b>' + esc(x.value) + "</b>" + esc(x.label) + "</span>"; }).join("");
        f.appendChild(k);
      }
      if (tr.tools && tr.tools.length) { var tb = document.createElement("div"); f.appendChild(tb); bars(tb, tr.tools, {}); }
      if (tr.story && tr.story.length) {
        var stc = document.createElement("div"); stc.className = "viz-story";
        tr.story.forEach(function (b, i) {
          if (i > 0) {
            var c = document.createElement("div");
            c.className = "viz-story-because" + (b.because ? "" : " plain");
            c.textContent = b.because ? "↓ " + b.because : "↓";
            stc.appendChild(c);
          }
          var r = document.createElement("div"); r.className = "viz-story-beat";
          r.innerHTML = '<span class="viz-story-dot" style="background:' + (b.status ? statusColor(b.status) : "var(--st-muted)") + '"></span>' +
            '<span class="viz-story-text">' + esc(b.text) + "</span>";
          stc.appendChild(r);
        });
        f.appendChild(stc);
      }
      if (tr.steps && tr.steps.length) {
        var ol = document.createElement("div"); ol.className = "viz-agent-steps";
        ol.innerHTML = tr.steps.map(function (s) {
          return '<div class="viz-step"><span class="viz-step-dot" style="background:' + (s.status ? statusColor(s.status) : "var(--st-muted)") + '"></span>' +
            '<span class="viz-step-tool">' + esc(s.label) + "</span>" +
            (s.sub ? '<code class="viz-step-sub">' + esc(s.sub) + "</code>" : "") +
            (s.meta ? '<span class="viz-step-meta">' + esc(s.meta) + "</span>" : "") + "</div>";
        }).join("");
        f.appendChild(ol);
      }
      return f;
    }

    function draw() {
      if (!map.getBoundingClientRect) return;
      var base = map.getBoundingClientRect();
      if (!base.width) return;
      clear(svgLayer);
      svgLayer.appendChild(defsArrow());
      svgLayer.setAttribute("width", base.width); svgLayer.setAttribute("height", base.height);
      svgLayer.setAttribute("viewBox", "0 0 " + base.width + " " + base.height);
      edges.forEach(function (e) {
        var a = cardById[e.from], b = cardById[e.to]; if (!a || !b || !a.getBoundingClientRect) return;
        var ra = a.getBoundingClientRect(), rb = b.getBoundingClientRect();
        var x1 = ra.left + ra.width / 2 - base.left, y1 = ra.bottom - base.top;
        var x2 = rb.left + rb.width / 2 - base.left, y2 = rb.top - base.top;
        var my = (y1 + y2) / 2;
        svgLayer.appendChild(svg("path", { d: "M" + x1 + "," + y1 + " C" + x1 + "," + my + " " + x2 + "," + my + " " + x2 + "," + y2, class: "viz-edge", "marker-end": "url(#viz-arrow)" }));
        if (e.label) {
          var mx = (x1 + x2) / 2, w = String(e.label).length * 6 + 12;
          if (e.status) svgLayer.appendChild(svg("rect", { x: mx - w / 2, y: my - 9, width: w, height: 17, rx: 8, class: "viz-edge-pill", style: "stroke:" + statusColor(e.status) }));
          else svgLayer.appendChild(svg("rect", { x: mx - w / 2, y: my - 9, width: w, height: 17, rx: 8, class: "viz-edge-pill" }));
          svgLayer.appendChild(svg("text", { x: mx, y: my + 3, class: "viz-edge-plabel", "text-anchor": "middle" }, [txt(e.label)]));
        }
      });
    }
    function schedule() { if (typeof requestAnimationFrame === "function") requestAnimationFrame(draw); else draw(); }
    schedule();
    nodes.forEach(function (n) { var c = cardById[n.id]; if (c && c.addEventListener) c.addEventListener("toggle", schedule); });
    if (typeof window !== "undefined" && window.addEventListener) window.addEventListener("resize", schedule);
    return host;
  }

  window.VIZ = {
    kpis: kpis, table: table, bars: bars, donut: donut,
    dag: dag, graph: graph, timeline: timeline, heat: heat, tree: tree, agentmap: agentmap,
    statusColor: statusColor, catColor: catColor, esc: esc, STATUS: STATUS,
  };
})();
