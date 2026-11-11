/*
 * The admin console's script: every function and handler the pages used to
 * carry inline, in one file served from this origin (/static/admin.js), so a
 * page needs no inline script and no inline event handler and a strict
 * Content-Security-Policy (script-src 'self', no 'unsafe-inline', no
 * 'unsafe-eval') can be applied to the console. Behaviour is bound through
 * data-* attributes and delegated listeners:
 *
 *   data-show-tab="<name>"            budgets: show one tab panel
 *   data-filter-cards="<container>"   filter cards by the typed text (input)
 *   data-submit-on-change             a select that submits its form on change
 *   data-confirm="<message>"          a submit that asks before it goes ahead
 *   data-submit-failover              models: save the failover chain
 *   data-edit-failover data-primary data-chain   models: load a chain into the form
 *   data-edit-alias data-alias data-target       models: load an alias into the form
 *   data-toggle-group="<gid>"         keys: show or hide a group's old keys
 *   data-reset-after-request[="successful"]      a form reset after its htmx request
 *   data-copy="<text>" [data-copied-label] [data-copy-label]   copy to the clipboard, label swapped briefly
 *   data-clear-html="<id>"            empty an element's content on click
 *   data-toggle-date-range            budgets: a window select that shows the date-range fields for 'total'
 *   [data-scroll-highlight="<id>"], template[data-prepend-into="<table id>"], template[data-prepend-child-into="<list id>"]
 *                                     markers a swapped fragment carries for work done after the swap
 *
 * htmx runs with allowEval off (the htmx-config meta in base.html), so no
 * hx-on attribute is used; the after-request reset above replaces them.
 * Charts in htmx-swapped panels read their data from data-chart-data
 * attributes and are drawn on load and after every swap.
 */
function filterCards(containerId, q) {
    q = q.toLowerCase();
    document.querySelectorAll('#' + containerId + ' > [id]').forEach(function(card) {
        var h = card.querySelector('h3');
        var text = (h ? h.textContent : card.textContent).toLowerCase();
        card.style.display = (!q || text.includes(q)) ? '' : 'none';
    });
}

function cellValue(cell) {
    return cell ? (cell.getAttribute('data-sort-value') || cell.textContent.trim()) : '';
}

function initSortable(root) {
    var tables = (root || document).querySelectorAll('table');
    tables.forEach(function(table) {
        // Add filter input above table if not already present
        if (!table._filterInit) {
            table._filterInit = true;
            var wrapper = document.createElement('div');
            wrapper.style.cssText = 'margin-bottom:0.5rem;';
            var input = document.createElement('input');
            input.type = 'search';
            input.placeholder = 'Filter rows…';
            input.style.cssText = 'padding:0.35rem 0.6rem;border:1px solid #ccc;border-radius:4px;font-size:0.85rem;width:220px;';
            input.addEventListener('input', function() {
                var q = input.value.toLowerCase();
                table.querySelectorAll('tbody tr').forEach(function(row) {
                    var text = row.textContent.toLowerCase();
                    row.style.display = (!q || text.includes(q)) ? '' : 'none';
                });
            });
            wrapper.appendChild(input);
            table.parentNode.insertBefore(wrapper, table);
        }

        var ths = table.querySelectorAll('thead th');
        ths.forEach(function(th, colIdx) {
            if (th._sortInit) return;
            th._sortInit = true;
            th.addEventListener('click', function() {
                var dir = th.getAttribute('data-sort-dir') === 'asc' ? 'desc' : 'asc';
                ths.forEach(function(t) {
                    t.removeAttribute('data-sort-dir');
                    t.textContent = t.textContent.replace(/ [↑↓]$/, '');
                });
                th.setAttribute('data-sort-dir', dir);
                th.textContent = th.textContent + (dir === 'asc' ? ' ↑' : ' ↓');

                // Collect ALL rows from ALL tbodys, consolidate into first tbody
                var tbodies = Array.from(table.querySelectorAll('tbody'));
                if (!tbodies.length) return;
                var rows = [];
                tbodies.forEach(function(tb) {
                    Array.from(tb.querySelectorAll('tr')).forEach(function(r) { rows.push(r); });
                });
                rows.sort(function(a, b) {
                    var aVal = cellValue(a.querySelectorAll('td')[colIdx]);
                    var bVal = cellValue(b.querySelectorAll('td')[colIdx]);
                    var cmp;
                    // Try date parse first (handles "YYYY-MM-DD HH:MM:SS" timestamps)
                    var aDate = Date.parse(aVal.replace(' ', 'T'));
                    var bDate = Date.parse(bVal.replace(' ', 'T'));
                    if (!isNaN(aDate) && !isNaN(bDate)) {
                        cmp = aDate - bDate;
                    } else {
                        // Strip currency symbols then try numeric
                        var aNum = parseFloat(aVal.replace(/[^0-9.]/g, ''));
                        var bNum = parseFloat(bVal.replace(/[^0-9.]/g, ''));
                        cmp = (!isNaN(aNum) && !isNaN(bNum))
                            ? aNum - bNum
                            : aVal.localeCompare(bVal);
                    }
                    return dir === 'asc' ? cmp : -cmp;
                });
                var primary = tbodies[0];
                rows.forEach(function(r) { primary.appendChild(r); });
                // Remove now-empty extra tbodys
                tbodies.slice(1).forEach(function(tb) {
                    if (!tb.querySelector('tr')) tb.remove();
                });
            });
        });
    });
}

document.addEventListener('DOMContentLoaded', function() {
    // Highlight active nav link
    var path = window.location.pathname;
    document.querySelectorAll('nav a[href]').forEach(function(a) {
        var href = a.getAttribute('href');
        if (href === '/admin' ? path === '/admin' : path.startsWith(href)) {
            a.classList.add('active');
        }
    });

    initSortable();
    document.querySelectorAll('table').forEach(function(table) {
        var ths = table.querySelectorAll('thead th');
        ths.forEach(function(th) {
            if (/created/i.test(th.textContent)) {
                th.setAttribute('data-sort-dir', 'asc');
                th.click();
            }
        });
    });
});

document.addEventListener('htmx:afterSwap', function(evt) {
    initSortable(evt.detail.target);
});

// ----- budgets -----
function showTab(name) {
    ['global','projects','users','groups'].forEach(function(t) {
        document.getElementById('tab-' + t).style.display = (t === name) ? '' : 'none';
        var btn = document.getElementById('tab-btn-' + t);
        btn.className = (t === name) ? 'btn btn-primary' : 'btn btn-secondary';
        btn.style.fontSize = '0.85rem';
    });
}

// ----- keys -----
function toggleGroup(gid) {
    var rows = document.querySelectorAll('.sub-' + gid);
    var btn = document.getElementById('toggle-' + gid);
    var show = rows.length > 0 && rows[0].style.display === 'none';
    rows.forEach(function(r) { r.style.display = show ? 'table-row' : 'none'; });
    if (btn) {
        var count = btn.dataset.count;
        btn.textContent = (show ? '▼ ' : '▶ ') + count + ' old key' + (count == 1 ? '' : 's');
    }
}

// ----- models -----
function editAlias(alias, target) {
  var form = document.querySelector('form[hx-post="/admin/aliases"]');
  form.querySelector('input[name="alias"]').value = alias;
  form.querySelector('input[name="target"]').value = target;
  form.querySelector('input[name="alias"]').focus();
}

// Populate the datalist from the catalog API (the models page only).
function initModelCatalog() {
  if (!document.getElementById('available-models-list')) return;
  (function() {
  fetch('/admin/api/models/available')
    .then(function(r) {
      if (!r.ok) throw new Error('HTTP ' + r.status);
      return r.json();
    })
    .then(function(data) {
      var datalist = document.getElementById('available-models-list');
      var warning = document.getElementById('catalog-warning');
      var models = [];

      if (!data.providers) {
        warning.style.display = 'block';
        return;
      }

      // Extract all models from all providers
      Object.keys(data.providers).forEach(function(providerName) {
        var provider = data.providers[providerName];
        if (provider.supported && provider.models && Array.isArray(provider.models)) {
          provider.models.forEach(function(m) {
            // CatalogModel has 'provider' and 'name' fields
            if (m.provider && m.name) {
              models.push(m.provider + '/' + m.name);
            }
          });
        }
      });

      if (models.length === 0) {
        warning.style.display = 'block';
        return;
      }

      // Populate datalist
      models.forEach(function(modelId) {
        var option = document.createElement('option');
        option.value = modelId;
        datalist.appendChild(option);
      });
    })
    .catch(function(err) {
      console.warn('Failed to load model catalog:', err);
      document.getElementById('catalog-warning').style.display = 'block';
    });
})();
}

function submitFailover() {
  var primary = document.getElementById('failover-primary').value.trim();
  var fallbacks = document.getElementById('failover-fallbacks').value.trim();
  if (!primary) { alert('Primary model is required.'); return; }

  var formData = new FormData();
  formData.append('fallbacks', fallbacks);

  fetch('/admin/models/' + encodeURIComponent(primary) + '/failovers', {
    method: 'POST',
    body: new URLSearchParams(formData),
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' }
  })
  .then(function(r) { return r.text(); })
  .then(function(html) {
    document.getElementById('failover-result').innerHTML = html;
    setTimeout(function() { window.location.reload(); }, 800);
  });
}

function editFailover(primary, chain) {
  document.getElementById('failover-primary').value = primary;
  document.getElementById('failover-fallbacks').value = chain;
  document.getElementById('failover-primary').focus();
  document.getElementById('failover-form').scrollIntoView({ behavior: 'smooth', block: 'center' });
}

// ----- cache: hit-rate chart -----
function initHitChart() {
    if (!document.getElementById('hitchart')) return;

(function() {
    var data = JSON.parse(document.getElementById('hitdata').textContent);
    var svg = d3.select('#hitchart');
    var box = svg.node().getBoundingClientRect();
    var w = box.width || 800, h = 220;
    var m = {top: 10, right: 20, bottom: 30, left: 40};
    var x = d3.scalePoint().domain(data.map(function(d) { return d.day; })).range([m.left, w - m.right]);
    var y = d3.scaleLinear().domain([0, 100]).range([h - m.bottom, m.top]);
    svg.append('g').attr('transform', 'translate(0,' + (h - m.bottom) + ')')
       .call(d3.axisBottom(x).tickValues(x.domain().filter(function(_, i) {
           return i % Math.ceil(data.length / 8) === 0;
       })));
    svg.append('g').attr('transform', 'translate(' + m.left + ',0)')
       .call(d3.axisLeft(y).ticks(5).tickFormat(function(v) { return v + '%'; }));
    var line = d3.line().x(function(d) { return x(d.day); }).y(function(d) { return y(d.hit_rate); });
    svg.append('path').datum(data).attr('fill', 'none')
       .attr('stroke', '#27ae60').attr('stroke-width', 2).attr('d', line);
    svg.selectAll('circle').data(data).enter().append('circle')
       .attr('cx', function(d) { return x(d.day); })
       .attr('cy', function(d) { return y(d.hit_rate); })
       .attr('r', 3).attr('fill', '#27ae60')
       .append('title').text(function(d) { return d.day + ': ' + d.hit_rate + '% of ' + d.requests + ' requests'; });
})();
}

// ----- compare panels -----
function initComparePanels() {
    if (!document.getElementById('compare-bars-chart')) return;

(function() {
    var BLUE = '#1a1a2e', TEAL = '#4ecdc4', ORANGE = '#e8703a', GRAY = '#999';

    function clearSvg(el) {
        while (el.firstChild) el.removeChild(el.firstChild);
    }

    function noData(el, text) {
        clearSvg(el);
        var p = document.createElement('p');
        p.setAttribute('style', 'color:#999;text-align:center;padding-top:2rem;font-size:0.85rem;');
        p.textContent = text || 'No data';
        el.appendChild(p);
    }

    // Parse the escaped JSON the template put on the element, or null.
    function chartData(el) {
        var raw = el.getAttribute('data-chart-data');
        try { return JSON.parse(raw); } catch(e) { return null; }
    }

    function frame(el, margin, H) {
        var W = el.offsetWidth || 320;
        var svg = d3.select(el).append('svg')
            .attr('width', W).attr('height', H)
            .append('g').attr('transform', 'translate(' + margin.left + ',' + margin.top + ')');
        return { svg: svg, w: W - margin.left - margin.right, h: H - margin.top - margin.bottom };
    }

    function legend(svg, x, y, entries) {
        var g = svg.append('g').attr('transform', 'translate(' + x + ',' + y + ')');
        entries.forEach(function(e, i) {
            var row = g.append('g').attr('transform', 'translate(' + (i * 110) + ',0)');
            row.append('rect').attr('width', 12).attr('height', 12).attr('fill', e.color);
            row.append('text').attr('x', 16).attr('y', 10).style('font-size', '0.7rem')
                .text(e.label.length > 14 ? e.label.slice(0, 13) + '…' : e.label);
        });
    }

    function fmtMoney(v) {
        if (v >= 1) return '$' + v.toFixed(2);
        if (v >= 0.01) return '$' + v.toFixed(4);
        return (v * 100).toFixed(4) + 'c';
    }
    function fmtNum(v) { return d3.format(',.1f')(v); }
    function fmtMs(v) { return d3.format(',.0f')(v) + ' ms'; }

    // Per-request cost, tokens in/out and mean latency, A beside B. Each
    // metric is normalised to its own A/B maximum so four unrelated units
    // share one panel; the true value is written above each bar.
    function initBarsChart() {
        var el = document.getElementById('compare-bars-chart');
        if (!el || typeof d3 === 'undefined') return;
        clearSvg(el);
        var data = chartData(el);
        if (!data || !data.a || !data.b) { noData(el); return; }
        if (!data.a.requests || !data.b.requests) { noData(el); return; }

        var metrics = [
            { key: 'uncached_cost_per_request', label: 'Uncached cost / req', fmt: fmtMoney },
            { key: 'cost_per_request',       label: 'Cost / req',   fmt: fmtMoney },
            { key: 'tokens_in_per_request',  label: 'Tokens in',    fmt: fmtNum },
            { key: 'tokens_out_per_request', label: 'Tokens out',   fmt: fmtNum },
            { key: 'mean_ms',                label: 'Mean latency', fmt: fmtMs }
        ];
        var arms = [{ id: 'a', color: BLUE, label: data.a.label }, { id: 'b', color: TEAL, label: data.b.label }];

        var f = frame(el, {top:28, right:10, bottom:30, left:10}, 240);
        var x0 = d3.scaleBand().domain(metrics.map(function(m) { return m.key; })).range([0, f.w]).padding(0.25);
        var x1 = d3.scaleBand().domain(['a', 'b']).range([0, x0.bandwidth()]).padding(0.1);
        var y = d3.scaleLinear().domain([0, 1]).range([f.h, 0]);

        f.svg.append('g').attr('transform', 'translate(0,' + f.h + ')')
            .call(d3.axisBottom(x0).tickFormat(function(k) {
                return metrics.filter(function(m) { return m.key === k; })[0].label;
            }))
            .selectAll('text').style('font-size', '0.7rem');

        metrics.forEach(function(m) {
            var va = data.a[m.key], vb = data.b[m.key];
            var max = Math.max(va == null ? 0 : va, vb == null ? 0 : vb);
            arms.forEach(function(arm) {
                var v = arm.id === 'a' ? va : vb;
                var norm = (v == null || max <= 0) ? 0 : v / max;
                var bx = x0(m.key) + x1(arm.id);
                f.svg.append('rect')
                    .attr('x', bx).attr('width', x1.bandwidth())
                    .attr('y', y(norm)).attr('height', f.h - y(norm))
                    .attr('fill', v == null ? GRAY : arm.color);
                f.svg.append('text')
                    .attr('x', bx + x1.bandwidth() / 2).attr('y', y(norm) - 4)
                    .attr('text-anchor', 'middle').style('font-size', '0.65rem').attr('fill', '#555')
                    .text(v == null ? '—' : m.fmt(v));
            });
        });
        legend(f.svg, 0, -24, arms);
    }

    // Daily uncached cost of both arms over the window, one line each, so a
    // rerun served from cache doesn't dip below the run that paid for it.
    function initDailyChart() {
        var el = document.getElementById('compare-daily-chart');
        if (!el || typeof d3 === 'undefined') return;
        clearSvg(el);
        var data = chartData(el);
        if (!data || !data.a || !data.b) { noData(el); return; }
        if (!data.a.requests || !data.b.requests) { noData(el); return; }

        var parseDate = d3.timeParse('%Y-%m-%d');
        function points(arm) {
            return (arm.series || []).map(function(d) { return { date: parseDate(d.day), v: d.uncached_cost_usd }; })
                .filter(function(p) { return p.date; });
        }
        var pa = points(data.a), pb = points(data.b);
        var all = pa.concat(pb);
        if (all.length === 0) { noData(el); return; }

        var f = frame(el, {top:28, right:20, bottom:36, left:60}, 240);
        var x = d3.scaleTime().domain(d3.extent(all, function(p) { return p.date; })).range([0, f.w]);
        var y = d3.scaleLinear().domain([0, (d3.max(all, function(p) { return p.v; }) || 1) * 1.05]).range([f.h, 0]);

        f.svg.append('g').attr('transform', 'translate(0,' + f.h + ')')
            // Whole days only: a short series otherwise gets sub-day ticks that
            // all print the same date.
            .call(d3.axisBottom(x).ticks(d3.timeDay.every(Math.max(1, Math.ceil(d3.timeDay.count(x.domain()[0], x.domain()[1]) / 6)))).tickFormat(d3.timeFormat('%m/%d')))
            .selectAll('text').style('font-size', '0.7rem');
        f.svg.append('g')
            .call(d3.axisLeft(y).ticks(5).tickFormat(function(d) { return '$' + d.toFixed(2); }))
            .selectAll('text').style('font-size', '0.7rem');

        var line = d3.line().x(function(p) { return x(p.date); }).y(function(p) { return y(p.v); });
        [{ pts: pa, color: BLUE }, { pts: pb, color: TEAL }].forEach(function(s) {
            if (s.pts.length === 0) return;
            f.svg.append('path').datum(s.pts)
                .attr('fill', 'none').attr('stroke', s.color).attr('stroke-width', 2).attr('d', line);
            // A single day has no line to see; mark the point.
            f.svg.selectAll(null).data(s.pts).enter().append('circle')
                .attr('cx', function(p) { return x(p.date); }).attr('cy', function(p) { return y(p.v); })
                .attr('r', s.pts.length === 1 ? 4 : 2.5).attr('fill', s.color);
        });
        legend(f.svg, 0, -24, [{ label: data.a.label, color: BLUE }, { label: data.b.label, color: TEAL }]);
    }

    // p50 and p95 for each arm as paired bars, in milliseconds.
    function initLatencyChart() {
        var el = document.getElementById('compare-latency-chart');
        if (!el || typeof d3 === 'undefined') return;
        clearSvg(el);
        var data = chartData(el);
        if (!data || !data.a || !data.b) { noData(el); return; }
        if (!data.a.samples && !data.b.samples) { noData(el, 'No latency samples'); return; }

        var arms = [
            { id: 'a', label: data.a.label, samples: data.a.samples, p50: data.a.p50_ms, p95: data.a.p95_ms },
            { id: 'b', label: data.b.label, samples: data.b.samples, p50: data.b.p50_ms, p95: data.b.p95_ms }
        ];
        var f = frame(el, {top:28, right:20, bottom:30, left:60}, 220);
        var x0 = d3.scaleBand().domain(['a', 'b']).range([0, f.w]).padding(0.35);
        var x1 = d3.scaleBand().domain(['p50', 'p95']).range([0, x0.bandwidth()]).padding(0.1);
        var max = d3.max(arms, function(a) { return Math.max(a.p50 || 0, a.p95 || 0); }) || 1;
        var y = d3.scaleLinear().domain([0, max * 1.15]).range([f.h, 0]);

        f.svg.append('g').attr('transform', 'translate(0,' + f.h + ')')
            .call(d3.axisBottom(x0).tickFormat(function(id) {
                var a = arms.filter(function(a) { return a.id === id; })[0];
                return (id.toUpperCase() + ': ' + a.label).slice(0, 28);
            }))
            .selectAll('text').style('font-size', '0.7rem');
        f.svg.append('g')
            .call(d3.axisLeft(y).ticks(5).tickFormat(function(d) { return d + ' ms'; }))
            .selectAll('text').style('font-size', '0.7rem');

        arms.forEach(function(arm) {
            if (!arm.samples) {
                f.svg.append('text')
                    .attr('x', x0(arm.id) + x0.bandwidth() / 2).attr('y', f.h - 8)
                    .attr('text-anchor', 'middle').attr('fill', GRAY).style('font-size', '0.75rem')
                    .text('no latency samples');
                return;
            }
            ['p50', 'p95'].forEach(function(q) {
                var v = arm[q];
                if (v == null) return;
                var bx = x0(arm.id) + x1(q);
                f.svg.append('rect')
                    .attr('x', bx).attr('width', x1.bandwidth())
                    .attr('y', y(v)).attr('height', f.h - y(v))
                    .attr('fill', q === 'p50' ? BLUE : ORANGE);
                f.svg.append('text')
                    .attr('x', bx + x1.bandwidth() / 2).attr('y', y(v) - 4)
                    .attr('text-anchor', 'middle').style('font-size', '0.65rem').attr('fill', '#555')
                    .text(q + ' ' + fmtMs(v));
            });
        });
        legend(f.svg, 0, -24, [{ label: 'p50', color: BLUE }, { label: 'p95', color: ORANGE }]);
    }

    function initCharts() {
        initBarsChart();
        initDailyChart();
        initLatencyChart();
    }

    // This script arrives with every htmx swap into #panels. htmx dispatches
    // htmx:afterSwap before this settle-time script executes, so the FIRST
    // swap's afterSwap has already fired by the time we get here -- render
    // directly just this once, then let the listener handle every later
    // swap (it's already bound before any subsequent afterSwap fires).
    if (!window.__compareChartsBound) {
        window.__compareChartsBound = true;
        initCharts();
        document.addEventListener('htmx:afterSwap', function(e) {
            if (e.detail && e.detail.target && e.detail.target.id === 'panels') {
                initCharts();
            }
        });
    }
})();
}

// ----- reports panels -----
function initReportsPanels() {
    if (!document.getElementById('burndown-chart')) return;

(function() {
    var BLUE = '#1a1a2e', TEAL = '#4ecdc4', ORANGE = '#e8703a', GRAY = '#999';

    function clearSvg(el) {
        while (el.firstChild) el.removeChild(el.firstChild);
    }

    function noData(el) {
        clearSvg(el);
        el.innerHTML = '<p style="color:#999;text-align:center;padding-top:2rem;font-size:0.85rem;">No data</p>';
    }

    function initBurndownChart() {
        var el = document.getElementById('burndown-chart');
        if (!el || typeof d3 === 'undefined') return;
        clearSvg(el);
        var raw = el.getAttribute('data-chart-data');
        var data;
        try { data = JSON.parse(raw); } catch(e) { return; }
        var series = (data.series || []);
        var hasBudget = data.limit != null;

        // Update the panel title
        var titleEl = el.closest('.stat-card') && el.closest('.stat-card').querySelector('h3');
        if (titleEl) titleEl.textContent = 'Budget Remaining';

        if (series.length === 0 && !hasBudget) { noData(el); return; }

        var margin = {top:10, right:20, bottom:40, left:60};
        var W = el.offsetWidth || 320;
        var H = 220;
        var w = W - margin.left - margin.right;
        var h = H - margin.top - margin.bottom;

        var svg = d3.select(el).append('svg')
            .attr('width', W).attr('height', H)
            .append('g').attr('transform', 'translate(' + margin.left + ',' + margin.top + ')');

        var parseDate = d3.timeParse('%Y-%m-%d');

        // Compute cumulative spend, then remaining = limit - cumulative.
        // If no budget limit, use total spend as the ceiling so the line still burns down.
        var totalSpend = series.reduce(function(s, d) { return s + d[1]; }, 0);
        var ceiling = hasBudget ? data.limit : totalSpend;

        var pts = [];
        if (data.start) {
            var startDate = parseDate(data.start);
            if (startDate) pts.push({ date: startDate, v: ceiling });
        }
        var cumul = 0;
        series.forEach(function(d) {
            var dt = parseDate(d[0]);
            if (dt) {
                cumul += d[1];
                pts.push({ date: dt, v: ceiling - cumul });
            }
        });

        if (pts.length === 0) { noData(el); return; }

        // x domain: period start to today
        var today = new Date();
        today.setHours(0, 0, 0, 0);
        var xDomain = [pts[0].date, d3.max([pts[pts.length-1].date, today])];
        var xScale = d3.scaleTime().domain(xDomain).range([0, w]);

        // y domain: 0 to ceiling (allow slight negative for over-budget)
        var yMin = Math.min(0, d3.min(pts, function(d) { return d.v; }) - 1);
        var yMax = ceiling * 1.05;
        var yScale = d3.scaleLinear().domain([yMin, yMax]).range([h, 0]);

        svg.append('g').attr('transform', 'translate(0,' + h + ')')
            .call(d3.axisBottom(xScale).ticks(5).tickFormat(d3.timeFormat('%m/%d')))
            .selectAll('text').style('font-size', '0.7rem');

        svg.append('g')
            .call(d3.axisLeft(yScale).ticks(5).tickFormat(function(d) { return '$' + d.toFixed(2); }))
            .selectAll('text').style('font-size', '0.7rem');

        // Zero line (budget exhausted)
        var zy = yScale(0);
        svg.append('line')
            .attr('x1', 0).attr('x2', w)
            .attr('y1', zy).attr('y2', zy)
            .attr('stroke', ORANGE).attr('stroke-width', 1)
            .attr('stroke-dasharray', '4,3');

        // Line colour: red if last remaining < 20% of ceiling, else blue
        var lastV = pts.length ? pts[pts.length-1].v : yMax;
        var lineColor = (lastV < ceiling * 0.2) ? '#c0392b' : BLUE;

        var line = d3.line()
            .x(function(d) { return xScale(d.date); })
            .y(function(d) { return yScale(d.v); });

        svg.append('path').datum(pts)
            .attr('fill', 'none')
            .attr('stroke', lineColor)
            .attr('stroke-width', 2)
            .attr('d', line);

        // Label current remaining
        if (pts.length > 1) {
            var last = pts[pts.length - 1];
            svg.append('text')
                .attr('x', xScale(last.date) + 4)
                .attr('y', yScale(last.v) + 4)
                .attr('fill', lineColor)
                .style('font-size', '0.7rem')
                .text('$' + last.v.toFixed(2) + ' left');
        }
    }

    function initTopModelsChart() {
        var el = document.getElementById('top-models-chart');
        if (!el || typeof d3 === 'undefined') return;
        clearSvg(el);
        var raw = el.getAttribute('data-chart-data');
        var data;
        try { data = JSON.parse(raw); } catch(e) { return; }
        if (!data || data.length === 0) { noData(el); return; }

        var margin = {top:10, right:20, bottom:10, left:160};
        var W = el.offsetWidth || 320;
        var H = 220;
        var w = W - margin.left - margin.right;
        var h = H - margin.top - margin.bottom;

        var svg = d3.select(el).append('svg')
            .attr('width', W).attr('height', H)
            .append('g').attr('transform', 'translate(' + margin.left + ',' + margin.top + ')');

        var top = data.slice(0, 8);
        var yScale = d3.scaleBand()
            .domain(top.map(function(d) { return d.model; }))
            .range([0, h]).padding(0.25);
        var xScale = d3.scaleLinear()
            .domain([0, d3.max(top, function(d) { return d.cost; }) || 1])
            .range([0, w]);

        svg.append('g').call(d3.axisLeft(yScale).tickSize(0))
            .selectAll('text').style('font-size', '0.7rem');
        svg.append('g').attr('transform', 'translate(0,' + h + ')')
            .call(d3.axisBottom(xScale).ticks(4).tickFormat(function(d) { return '$' + d.toFixed(2); }))
            .selectAll('text').style('font-size', '0.7rem');

        svg.selectAll('.bar').data(top).enter().append('rect')
            .attr('class', 'bar')
            .attr('y', function(d) { return yScale(d.model); })
            .attr('height', yScale.bandwidth())
            .attr('x', 0)
            .attr('width', function(d) { return xScale(d.cost); })
            .attr('fill', TEAL);
    }

    function initTokenUsageChart() {
        var el = document.getElementById('token-usage-chart');
        if (!el || typeof d3 === 'undefined') return;
        clearSvg(el);
        var raw = el.getAttribute('data-chart-data');
        var data;
        try { data = JSON.parse(raw); } catch(e) { return; }
        if (!data || data.length === 0) { noData(el); return; }

        var margin = {top:10, right:80, bottom:40, left:70};
        var W = el.offsetWidth || 320;
        var H = 220;
        var w = W - margin.left - margin.right;
        var h = H - margin.top - margin.bottom;

        var svg = d3.select(el).append('svg')
            .attr('width', W).attr('height', H)
            .append('g').attr('transform', 'translate(' + margin.left + ',' + margin.top + ')');

        var keys = ['tokens_in', 'tokens_out'];
        var stack = d3.stack().keys(keys)(data);

        var xScale = d3.scaleBand()
            .domain(data.map(function(d) { return d.user; }))
            .range([0, w]).padding(0.3);
        var yMax = d3.max(data, function(d) { return d.tokens_in + d.tokens_out; }) || 1;
        var yScale = d3.scaleLinear().domain([0, yMax]).range([h, 0]);
        var color = d3.scaleOrdinal().domain(keys).range([BLUE, TEAL]);

        svg.append('g').attr('transform', 'translate(0,' + h + ')')
            .call(d3.axisBottom(xScale))
            .selectAll('text').attr('transform', 'rotate(-25)').attr('text-anchor', 'end')
            .style('font-size', '0.7rem');
        svg.append('g')
            .call(d3.axisLeft(yScale).ticks(5).tickFormat(d3.format('.2s')))
            .selectAll('text').style('font-size', '0.7rem');

        svg.selectAll('.layer').data(stack).enter().append('g')
            .attr('fill', function(d) { return color(d.key); })
            .selectAll('rect').data(function(d) { return d; }).enter().append('rect')
            .attr('x', function(d) { return xScale(d.data.user); })
            .attr('y', function(d) { return yScale(d[1]); })
            .attr('height', function(d) { return yScale(d[0]) - yScale(d[1]); })
            .attr('width', xScale.bandwidth());

        // Legend
        var legend = svg.append('g').attr('transform', 'translate(' + (w + 8) + ',10)');
        keys.forEach(function(k, i) {
            var g = legend.append('g').attr('transform', 'translate(0,' + (i * 18) + ')');
            g.append('rect').attr('width', 12).attr('height', 12).attr('fill', color(k));
            g.append('text').attr('x', 16).attr('y', 10)
                .text(k === 'tokens_in' ? 'Out (Compl.)' : 'In (Prompts)')
                .style('font-size', '0.7rem');
        });
    }

    function initCharts() {
        initBurndownChart();
        initTopModelsChart();
        initTokenUsageChart();
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', initCharts);
    } else {
        initCharts();
    }

    document.addEventListener('htmx:afterSwap', function(e) {
        if (e.detail && e.detail.target && e.detail.target.id === 'panels') {
            initCharts();
        }
    });
})();
}

// ----- groups: re-apply the filter after htmx injects new cards -----
// Re-apply filter after HTMX injects new cards
document.addEventListener('htmx:afterSwap', function() {
    var inp = document.getElementById('group-filter');
    if (inp) filterCards('groups-list', inp.value);
});

// ----- delegated handlers for the data-* hooks -----
// Markup an htmx swap brings in may ask for work the fragment used to do with
// an inline script: scroll-and-highlight a row, or insert a <template>'s
// content into a table or list and let htmx process it.
function applySwapMarkers(root) {
    var scope = root || document;
    scope.querySelectorAll('[data-scroll-highlight]').forEach(function(marker) {
        var el = document.getElementById(marker.getAttribute('data-scroll-highlight'));
        marker.remove();
        if (!el) return;
        el.scrollIntoView({ behavior: 'smooth', block: 'center' });
        el.classList.add('row-highlight');
        setTimeout(function() { el.classList.remove('row-highlight'); }, 2500);
    });
    scope.querySelectorAll('template[data-prepend-into]').forEach(function(t) {
        var table = document.getElementById(t.getAttribute('data-prepend-into'));
        var el = t.content.firstElementChild ? t.content.firstElementChild.cloneNode(true) : null;
        t.remove();
        if (!table || !el) return;
        table.insertBefore(el, table.querySelector('tbody') || null);
        if (window.htmx) htmx.process(el);
    });
    scope.querySelectorAll('template[data-prepend-child-into]').forEach(function(t) {
        var list = document.getElementById(t.getAttribute('data-prepend-child-into'));
        var el = t.content.firstElementChild ? t.content.firstElementChild.cloneNode(true) : null;
        t.remove();
        if (!list || !el) return;
        list.prepend(el);
        if (window.htmx) htmx.process(el);
    });
}

document.addEventListener('click', function(evt) {
    var t = evt.target.closest ? evt.target.closest('[data-show-tab],[data-submit-failover],[data-edit-failover],[data-edit-alias],[data-toggle-group],[data-confirm],[data-copy],[data-clear-html]') : null;
    if (!t) return;
    if (t.hasAttribute('data-clear-html')) {
        var target = document.getElementById(t.getAttribute('data-clear-html'));
        if (target) target.innerHTML = '';
        return;
    }
    if (t.hasAttribute('data-copy')) {
        var text = t.getAttribute('data-copy');
        var done = t.getAttribute('data-copied-label') || '\u2713';
        var idle = t.getAttribute('data-copy-label') || t.textContent;
        navigator.clipboard.writeText(text).then(function() {
            t.textContent = done;
            setTimeout(function() { t.textContent = idle; }, 1500);
        });
        return;
    }
    if (t.hasAttribute('data-confirm')) {
        if (!window.confirm(t.getAttribute('data-confirm'))) { evt.preventDefault(); evt.stopImmediatePropagation(); }
        return;
    }
    if (t.hasAttribute('data-show-tab')) return showTab(t.getAttribute('data-show-tab'));
    if (t.hasAttribute('data-submit-failover')) return submitFailover();
    if (t.hasAttribute('data-edit-failover')) return editFailover(t.getAttribute('data-primary') || '', t.getAttribute('data-chain') || '');
    if (t.hasAttribute('data-edit-alias')) return editAlias(t.getAttribute('data-alias') || '', t.getAttribute('data-target') || '');
    if (t.hasAttribute('data-toggle-group')) return toggleGroup(t.getAttribute('data-toggle-group'));
});
document.addEventListener('input', function(evt) {
    var t = evt.target;
    if (t && t.hasAttribute && t.hasAttribute('data-filter-cards')) filterCards(t.getAttribute('data-filter-cards'), t.value);
});
document.addEventListener('change', function(evt) {
    var t = evt.target;
    if (!t || !t.hasAttribute) return;
    if (t.hasAttribute('data-submit-on-change') && t.form) t.form.submit();
    if (t.hasAttribute('data-toggle-date-range')) {
        var form = t.closest('form');
        var dr = form ? form.querySelector('.date-range-fields') : null;
        if (dr) dr.style.display = t.value === 'total' ? 'flex' : 'none';
    }
});
document.addEventListener('htmx:afterRequest', function(evt) {
    var form = evt.target && evt.target.closest ? evt.target.closest('[data-reset-after-request]') : null;
    if (!form || typeof form.reset !== 'function') return;
    var mode = form.getAttribute('data-reset-after-request');
    if (mode === 'successful' && !(evt.detail && evt.detail.successful)) return;
    form.reset();
});
document.addEventListener('DOMContentLoaded', function() {
    initModelCatalog();
    initHitChart();
    initComparePanels();
    initReportsPanels();
});
document.addEventListener('htmx:afterSwap', function(evt) {
    applySwapMarkers(evt.detail && evt.detail.target ? evt.detail.target : document);
    applySwapMarkers(document);
    initComparePanels();
    initReportsPanels();
});
