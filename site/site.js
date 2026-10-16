/* Elffuss Braid · braid.elffuss.com
   The language toggle, the copy buttons, the faded edge of code blocks that
   scroll (only those take keyboard focus), and the markers of the ratio
   table. No dependencies, no requests. Without this file the page is
   English and complete, every code block takes focus, and the toggle and
   the copy buttons stay hidden. */
(function () {
  'use strict';

  var KEY = 'braid-lang';
  var root = document.documentElement;
  var TITLE = {
    en: 'Elffuss Braid · version control for many agents at once',
    es: 'Elffuss Braid · control de versiones para muchos agentes a la vez'
  };
  var SAID = {
    en: { copied: 'Copied', failed: 'Could not copy: the text is selected instead' },
    es: { copied: 'Copiado', failed: 'No se pudo copiar: el texto queda seleccionado' }
  };

  function valid(value) {
    return value === 'en' || value === 'es' ? value : null;
  }
  function fromQuery() {
    try { return valid(new URLSearchParams(window.location.search).get('lang')); } catch (e) { return null; }
  }
  function fromStore() {
    try { return valid(window.localStorage.getItem(KEY)); } catch (e) { return null; }
  }
  function remember(value) {
    try { window.localStorage.setItem(KEY, value); } catch (e) { /* private mode: fine */ }
  }
  function fromBrowser() {
    var tag = (navigator.language || '').toLowerCase();
    return tag.indexOf('es') === 0 ? 'es' : null;
  }

  // ?lang=, then the stored choice, then a Spanish browser, then English.
  // Set before the body is parsed, so the other language never flashes.
  var lang = fromQuery() || fromStore() || fromBrowser() || 'en';
  root.setAttribute('lang', lang);

  function each(selector, fn) {
    var list = document.querySelectorAll(selector);
    for (var i = 0; i < list.length; i++) fn(list[i]);
  }

  // Boxes whose text keeps its lines and may scroll sideways. In the HTML
  // each is a labelled region that takes keyboard focus; edges() keeps that
  // only for the ones that do scroll at the current width.
  var SCROLLERS = '.code pre, .term-cols pre, .table-wrap';
  var scrollers = [];

  function apply(value) {
    lang = value;
    root.setAttribute('lang', value);
    document.title = TITLE[value];
    each('[data-set-lang]', function (button) {
      button.setAttribute('aria-pressed', String(button.getAttribute('data-set-lang') === value));
    });
    each('[data-aria-' + value + ']', function (el) {
      if (el.matches(SCROLLERS) && el.getAttribute('role') !== 'region') return;
      el.setAttribute('aria-label', el.getAttribute('data-aria-' + value));
    });
    allEdges(); // the other language has other widths
  }

  function keepQuery(value) {
    try {
      var url = new URL(window.location.href);
      if (url.searchParams.has('lang')) {
        url.searchParams.set('lang', value);
        window.history.replaceState(null, '', url.toString());
      }
    } catch (e) { /* old browser: the toggle still works */ }
  }

  function say(text) {
    var live = document.getElementById('live');
    if (!live) return;
    live.textContent = '';
    window.setTimeout(function () { live.textContent = text; }, 40);
  }

  function write(text) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text);
    }
    return new Promise(function (resolve, reject) {
      var area = document.createElement('textarea');
      area.value = text;
      area.setAttribute('readonly', '');
      area.style.position = 'fixed';
      area.style.top = '0';
      area.style.left = '0';
      area.style.opacity = '0';
      document.body.appendChild(area);
      area.select();
      var done = false;
      try { done = document.execCommand('copy'); } catch (e) { done = false; }
      document.body.removeChild(area);
      if (done) resolve(); else reject(new Error('copy'));
    });
  }

  function copy(button) {
    var block = button.closest('.code');
    var code = block && block.querySelector('pre code');
    if (!code) return;
    write(code.textContent).then(function () {
      button.classList.add('copied');
      window.clearTimeout(button._braidTimer);
      button._braidTimer = window.setTimeout(function () { button.classList.remove('copied'); }, 1800);
      say(SAID[lang].copied);
    }, function () {
      var range = document.createRange();
      range.selectNodeContents(code);
      var selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      say(SAID[lang].failed);
    });
  }

  // A marker per row of the ratio table, drawn from the table's own cells
  // on a log scale, with a line at 1.
  function markers() {
    var table = document.querySelector('#measured table.ratios');
    if (!table || !table.tBodies.length) return;
    var found = [];
    var rows = table.tBodies[0].rows;
    for (var i = 0; i < rows.length; i++) {
      var cell = rows[i].cells[1];
      var value = cell ? parseFloat(cell.textContent.replace(',', '.')) : NaN;
      if (isFinite(value) && value > 0) found.push({ cell: cell, value: value });
    }
    if (!found.length) return;
    var values = found.map(function (f) { return f.value; });
    var lo = Math.log(Math.min(0.1, Math.min.apply(null, values) / 1.5));
    var hi = Math.log(Math.max(10, Math.max.apply(null, values) * 1.5));
    function at(v) { return (Math.log(v) - lo) / (hi - lo) * 100; }
    var one = at(1);
    found.forEach(function (f) {
      var x = at(f.value);
      var mark = document.createElement('span');
      mark.className = 'mk';
      mark.setAttribute('aria-hidden', 'true');
      var line = document.createElement('span');
      line.className = 'mk-one';
      line.style.left = one.toFixed(2) + '%';
      var bar = document.createElement('span');
      bar.className = 'mk-bar';
      bar.style.left = Math.min(x, one).toFixed(2) + '%';
      bar.style.width = Math.abs(x - one).toFixed(2) + '%';
      var dot = document.createElement('span');
      dot.className = 'mk-dot';
      dot.style.left = x.toFixed(2) + '%';
      mark.appendChild(bar);
      mark.appendChild(line);
      mark.appendChild(dot);
      f.cell.appendChild(mark);
    });
    var legend = document.getElementById('mk-legend');
    if (legend) legend.hidden = false;
  }

  // A box that is wider than its text scrolls in it. Only then is it a
  // region to tab to, named in the page's language; otherwise it is plain
  // text, and keyboards and screen readers pass over it. The side that has
  // more is marked, and site.css fades the text there.
  function edges(el) {
    var max = el.scrollWidth - el.clientWidth;
    var scrolls = max > 1;
    if (scrolls) {
      el.setAttribute('tabindex', '0');
      el.setAttribute('role', 'region');
      var label = el.getAttribute('data-aria-' + lang);
      if (label) el.setAttribute('aria-label', label);
    } else if (el !== document.activeElement) {
      el.removeAttribute('tabindex');
      el.removeAttribute('role');
      el.removeAttribute('aria-label');
    }
    el.classList.toggle('more-right', scrolls && el.scrollLeft < max - 1);
    el.classList.toggle('more-left', scrolls && el.scrollLeft > 1);
  }
  function allEdges() {
    for (var i = 0; i < scrollers.length; i++) edges(scrollers[i]);
  }
  function watchEdges() {
    scrollers = document.querySelectorAll(SCROLLERS);
    for (var i = 0; i < scrollers.length; i++) {
      scrollers[i].addEventListener('scroll', function () { edges(this); }, { passive: true });
    }
    window.addEventListener('resize', allEdges);
    document.addEventListener('toggle', allEdges, true); // a <details> opened
    allEdges();
  }

  function start() {
    apply(lang);
    // Without this file these do nothing, so they only show up with it.
    each('button.copy, .lang', function (el) { el.hidden = false; });
    document.addEventListener('click', function (event) {
      var target = event.target;
      if (!target || !target.closest) return;
      var toggle = target.closest('[data-set-lang]');
      if (toggle) {
        var value = valid(toggle.getAttribute('data-set-lang'));
        if (value) {
          apply(value);
          remember(value);
          keepQuery(value);
        }
        return;
      }
      var button = target.closest('button.copy');
      if (button) copy(button);
    });
    markers();
    watchEdges();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
})();
