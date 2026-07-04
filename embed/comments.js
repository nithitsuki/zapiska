/**
 * comments.js — Embeddable comment widget for zapiska
 *
 * Usage:
 *   <script
 *     id="nc-comments"
 *     src="https://webmention.nithitsuki.com/embed/comments.js"
 *     data-path="/blog/hello-world"
 *     data-limit="50"
 *   ></script>
 *
 * The script fetches approved comments from the API and renders them
 * into an element with id "nc-comments" (or a sibling inserted element).
 */

(function () {
  'use strict';

  var script = document.querySelector('script#nc-comments') || document.getElementById('nc-comments');
  if (!script) return;

  var apiOrigin = script.getAttribute('data-api-origin');
  if (!apiOrigin) {
    var src = script.getAttribute('src');
    if (src) {
      var m = src.match(/^(https?:\/\/[^\/]+)/);
      if (m) apiOrigin = m[1];
    }
  }
  if (!apiOrigin) apiOrigin = window.location.origin;
  var path = script.getAttribute('data-path') || '/';
  var limit = parseInt(script.getAttribute('data-limit'), 10) || 50;
  var target = document.getElementById('nc-comments');
  if (!target) target = document.body;

  var apiUrl = apiOrigin + '/api/comments?path=' + encodeURIComponent(path) + '&limit=' + limit;

  // ── HTML sanitizer (defence-in-depth) ───────────────────────
  // If DOMPurify is loaded, use it; otherwise strip dangerous tags manually.
  function sanitizeHtml(html) {
    if (typeof DOMPurify !== 'undefined' && DOMPurify.sanitize) {
      return DOMPurify.sanitize(html, {
        ALLOWED_TAGS: ['a','p','b','i','em','strong','code','pre','br','ul','ol','li','blockquote'],
        ALLOWED_ATTR: ['href','target']
      });
    }
    return html
      .replace(/<script\b[^<]*(?:(?!<\/script>)<[^<]*)*<\/script>/gi, '')
      .replace(/<iframe\b[^<]*(?:(?!<\/iframe>)<[^<]*)*<\/iframe>/gi, '')
      .replace(/on\w+\s*=\s*"[^"]*"/gi, '')
      .replace(/on\w+\s*=\s*'[^']*'/gi, '')
      .replace(/on\w+\s*=\s*\S+/gi, '');
  }

  function escapeHtml(str) {
    if (!str) return '';
    return str
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function renderComment(c) {
    var name = escapeHtml(c.author_name);
    var url = c.author_url ? escapeHtml(c.author_url) : null;
    var avatar = c.author_avatar ? escapeHtml(c.author_avatar) : null;
    var content = sanitizeHtml(c.content || '');

    var el = document.createElement('div');
    el.className = 'nc-comment';

    var meta = document.createElement('div');
    meta.className = 'nc-meta';

    if (avatar) {
      var img = document.createElement('img');
      img.className = 'nc-avatar';
      img.src = avatar;
      img.alt = name;
      img.width = 32;
      img.height = 32;
      meta.appendChild(img);
    }

    if (url) {
      var a = document.createElement('a');
      a.className = 'nc-author';
      a.href = url;
      a.target = '_blank';
      a.rel = 'noopener noreferrer ugc';
      a.textContent = name;
      meta.appendChild(a);
    } else {
      var span = document.createElement('span');
      span.className = 'nc-author';
      span.textContent = name;
      meta.appendChild(span);
    }

    var date = document.createElement('time');
    date.className = 'nc-date';
    date.textContent = c.created_at ? c.created_at.slice(0, 10) : '';
    meta.appendChild(date);

    el.appendChild(meta);

    var body = document.createElement('div');
    body.className = 'nc-body';
    body.innerHTML = content;
    el.appendChild(body);

    return el;
  }

  function renderEmpty() {
    var el = document.createElement('p');
    el.className = 'nc-empty';
    el.textContent = 'No comments yet.';
    return el;
  }

  function renderError() {
    var el = document.createElement('p');
    el.className = 'nc-error';
    el.textContent = 'Comments could not be loaded.';
    return el;
  }

  // ── Fetch and render ───────────────────────────────────────

  fetch(apiUrl)
    .then(function (r) {
      if (!r.ok) throw new Error('HTTP ' + r.status);
      return r.json();
    })
    .then(function (data) {
      // Clear placeholder
      target.innerHTML = '';

      if (!data.comments || data.comments.length === 0) {
        target.appendChild(renderEmpty());
        return;
      }

      var heading = document.createElement('h3');
      heading.className = 'nc-heading';
      heading.textContent = 'Comments (' + (data.total || data.comments.length) + ')';
      target.appendChild(heading);

      data.comments.forEach(function (c) {
        target.appendChild(renderComment(c));
      });
    })
    .catch(function () {
      target.innerHTML = '';
      target.appendChild(renderError());
    });
})();
