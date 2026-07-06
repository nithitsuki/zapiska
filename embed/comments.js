/**
 * comments.js — Embeddable comment widget for zapiska
 *
 * Renders threaded comments with an inline reply form.
 *
 * Usage (minimal):
 *   <script id="nc-comments"
 *     src="/embed/comments.js"
 *     data-path="/blog/hello-world"></script>
 *
 * All data-* attributes are optional — see embed/README.md for the full list.
 *
 * To build your own frontend, just use the JSON API directly:
 *   GET  /api/comments?path=...   — fetch approved comments
 *   POST /api/comment              — submit a comment
 */
(function () {
  'use strict';

  var script = document.querySelector('script#nc-comments') ||
               document.getElementById('nc-comments');
  if (!script) return;

  // ── Helpers ────────────────────────────────────────────────

  function attr(name, fallback) {
    var v = script.getAttribute(name);
    return v !== null && v !== '' ? v : fallback;
  }

  function boolAttr(name, fallback) {
    var v = script.getAttribute(name);
    if (v === null) return fallback;
    return v === 'true' || v === '1' || v === 'yes';
  }

  function intAttr(name, fallback) {
    var v = script.getAttribute(name);
    if (v === null) return fallback;
    var n = parseInt(v, 10);
    return isNaN(n) ? fallback : n;
  }

  // ── Configuration (all driven by data-* attributes) ────────

  var origin;
  var srcOrigin = attr('data-api-origin');
  if (srcOrigin) {
    origin = srcOrigin;
  } else {
    var src = script.getAttribute('src');
    if (src) {
      var m = src.match(/^(https?:\/\/[^\/]+)/);
      if (m) origin = m[1];
    }
  }
  if (!origin) origin = window.location.origin;

  var path          = attr('data-path', '/');
  var limit         = intAttr('data-limit', 50);

  // Overridable text
  var headingText   = attr('data-heading-text', 'Comments (%d)');
  var emptyText     = attr('data-empty-text', 'No comments yet.');
  var errorText     = attr('data-error-text', 'Comments could not be loaded.');
  var replyText     = attr('data-reply-text', 'Reply');
  var submitText    = attr('data-submit-text', 'Submit');
  var cancelText    = attr('data-cancel-text', 'Cancel');
  var namePH        = attr('data-name-placeholder', 'Your name');
  var websitePH     = attr('data-website-placeholder', 'Website (optional)');
  var replyPH       = attr('data-reply-placeholder', 'Write your reply...');
  var pendingText   = attr('data-pending-text', 'Reply submitted (pending approval).');

  // Behavior flags
  var hideReplies   = boolAttr('data-hide-replies', false);
  var hideHeading   = boolAttr('data-hide-heading', false);
  var noStyles      = boolAttr('data-nostyles', false);
  var linkTarget    = attr('data-link-target', '_blank');

  // Turnstile
  var tsSitekey     = attr('data-turnstile-sitekey', '');

  // Avatar size (px)
  var avatarSize    = intAttr('data-avatar-size', 24);

  var target = document.getElementById('nc-comments');
  if (!target) target = document.body;

  var apiUrl = origin + '/api/comments?path=' +
               encodeURIComponent(path) + '&limit=' + limit;

  // ── Turnstile API loader (one-shot) ────────────────────────

  var turnstileLoaded = false;

  function ensureTurnstile(callback) {
    if (typeof turnstile !== 'undefined') { callback(); return; }
    if (turnstileLoaded) { setTimeout(callback, 500); return; }
    turnstileLoaded = true;
    var s = document.createElement('script');
    s.src = 'https://challenges.cloudflare.com/turnstile/v0/api.js';
    s.async = true;
    s.defer = true;
    s.onload = callback;
    document.head.appendChild(s);
  }

  // ── Styles (optional — suppressed with data-nostyles) ──────

  if (!noStyles) {
    (function injectStyles() {
      if (document.getElementById('nc-thread-styles')) return;
      var css = document.createElement('style');
      css.id = 'nc-thread-styles';
      css.textContent =
        '.nc-comment { margin-bottom: 0; }' +
        '.nc-thread { margin-left: 32px; border-left: 1px solid #ccc; padding-left: 12px; }' +
        '.nc-meta { display: flex; align-items: center; gap: 6px; margin-bottom: 4px; font-size: 13px; }' +
        '.nc-avatar { border-radius: 50%; object-fit: cover; flex-shrink: 0; }' +
        '.nc-author { font-weight: 600; text-decoration: none; color: #333; }' +
        '.nc-author:hover { text-decoration: underline; }' +
        '.nc-date { font-size: 12px; color: #888; }' +
        '.nc-body { font-size: 14px; line-height: 1.5; margin-bottom: 6px; }' +
        '.nc-body p { margin-bottom: 4px; }' +
        '.nc-body p:last-child { margin-bottom: 0; }' +
        '.nc-reply-btn { font-size: 12px; color: #555; cursor: pointer; border: none; background: none; padding: 0; font-family: inherit; }' +
        '.nc-reply-btn:hover { color: #000; text-decoration: underline; }' +
        '.nc-reply-form { margin: 8px 0 8px 32px; padding: 10px; border: 1px solid #ddd; background: #f9f9f9; }' +
        '.nc-reply-form input, .nc-reply-form textarea { display: block; width: 100%; margin-bottom: 6px; padding: 6px 8px; border: 1px solid #ccc; font: inherit; font-size: 13px; box-sizing: border-box; }' +
        '.nc-reply-form textarea { min-height: 60px; resize: vertical; }' +
        '.nc-reply-form .nc-form-actions { display: flex; gap: 6px; }' +
        '.nc-reply-form button { font: inherit; font-size: 12px; padding: 5px 12px; border: 1px solid #ccc; background: #fff; cursor: pointer; }' +
        '.nc-reply-form button:hover { background: #f0f0f0; }' +
        '.nc-reply-form .nc-submit { background: #333; color: #fff; border-color: #333; }' +
        '.nc-reply-form .nc-submit:hover { background: #555; }' +
        '.nc-reply-form .nc-submit:disabled { opacity: .5; cursor: default; }' +
        '.nc-error { color: #c00; }';
      document.head.appendChild(css);
    })();
  }

  // ── HTML sanitizer ─────────────────────────────────────────

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

  // ── Tree building ──────────────────────────────────────────

  function buildTree(comments) {
    var byId = {};
    var roots = [];

    comments.forEach(function (c) {
      byId[c.id] = { comment: c, children: [] };
    });

    comments.forEach(function (c) {
      var node = byId[c.id];
      if (c.parent_id && byId[c.parent_id]) {
        byId[c.parent_id].children.push(node);
      } else {
        roots.push(node);
      }
    });

    roots.sort(function (a, b) { return b.comment.id - a.comment.id; });
    Object.keys(byId).forEach(function (id) {
      byId[id].children.sort(function (a, b) { return a.comment.id - b.comment.id; });
    });

    return roots;
  }

  // ── Rendering ──────────────────────────────────────────────

  function renderCommentEl(c) {
    var name = escapeHtml(c.author_name);
    var url = c.author_url ? escapeHtml(c.author_url) : null;
    var avatar = c.author_avatar ? escapeHtml(c.author_avatar) : null;
    var content = sanitizeHtml(c.content || '');
    var date = c.created_at ? c.created_at.slice(0, 10) : '';

    var el = document.createElement('div');
    el.className = 'nc-comment';
    el.dataset.id = c.id;
    el.dataset.depth = c.depth;

    var meta = document.createElement('div');
    meta.className = 'nc-meta';

    if (avatar) {
      var img = document.createElement('img');
      img.className = 'nc-avatar';
      img.src = avatar;
      img.alt = name;
      img.width = avatarSize;
      img.height = avatarSize;
      img.loading = 'lazy';
      meta.appendChild(img);
    }

    if (url) {
      var a = document.createElement('a');
      a.className = 'nc-author';
      a.href = url;
      a.target = linkTarget;
      a.rel = 'noopener noreferrer ugc';
      a.textContent = name;
      meta.appendChild(a);
    } else {
      var span = document.createElement('span');
      span.className = 'nc-author';
      span.textContent = name;
      meta.appendChild(span);
    }

    var time = document.createElement('time');
    time.className = 'nc-date';
    time.textContent = date;
    meta.appendChild(time);

    el.appendChild(meta);

    var body = document.createElement('div');
    body.className = 'nc-body';
    body.innerHTML = content;
    el.appendChild(body);

    if (!hideReplies) {
      var replyBtn = document.createElement('button');
      replyBtn.className = 'nc-reply-btn';
      replyBtn.textContent = replyText;
      replyBtn.onclick = function () { toggleReplyForm(el, c.id, c.depth); };
      el.appendChild(replyBtn);
    }

    return el;
  }

  function renderNode(node) {
    var el = renderCommentEl(node.comment);

    if (node.children.length > 0) {
      var thread = document.createElement('div');
      thread.className = 'nc-thread';
      node.children.forEach(function (child) {
        thread.appendChild(renderNode(child));
      });
      el.appendChild(thread);
    }

    return el;
  }

  function toggleReplyForm(commentEl, parentId, parentDepth) {
    var existing = commentEl.querySelector('.nc-reply-form');
    if (existing) { existing.remove(); return; }

    document.querySelectorAll('.nc-reply-form').forEach(function (f) { f.remove(); });

    var form = document.createElement('div');
    form.className = 'nc-reply-form';

    var nameInput = document.createElement('input');
    nameInput.type = 'text';
    nameInput.placeholder = namePH;
    nameInput.required = true;

    var urlInput = document.createElement('input');
    urlInput.type = 'url';
    urlInput.placeholder = websitePH;

    var honeypot = document.createElement('input');
    honeypot.type = 'text';
    honeypot.name = 'website';
    honeypot.tabIndex = -1;
    honeypot.autocomplete = 'off';
    honeypot.style.cssText = 'position:absolute;left:-9999px;top:-9999px;width:1px;height:1px;opacity:0';

    var bodyTextarea = document.createElement('textarea');
    bodyTextarea.placeholder = replyPH;
    bodyTextarea.required = true;

    // Turnstile container (inserted before the actions)
    var tsWidgetId = null;
    var tsContainer = null;
    if (tsSitekey) {
      tsContainer = document.createElement('div');
      tsContainer.className = 'cf-turnstile';
      tsContainer.dataset.sitekey = tsSitekey;
    }

    var actions = document.createElement('div');
    actions.className = 'nc-form-actions';

    var submitBtn = document.createElement('button');
    submitBtn.className = 'nc-submit';
    submitBtn.textContent = submitText;

    var cancelBtn = document.createElement('button');
    cancelBtn.textContent = cancelText;
    cancelBtn.onclick = function () {
      if (tsWidgetId !== null && typeof turnstile !== 'undefined') {
        try { turnstile.remove(tsWidgetId); } catch (e) {}
      }
      form.remove();
    };

    actions.appendChild(submitBtn);
    actions.appendChild(cancelBtn);

    form.appendChild(nameInput);
    form.appendChild(urlInput);
    form.appendChild(honeypot);
    form.appendChild(bodyTextarea);
    if (tsContainer) form.appendChild(tsContainer);
    form.appendChild(actions);

    commentEl.appendChild(form);
    nameInput.focus();

    // Render Turnstile widget into its container
    if (tsContainer && tsSitekey) {
      ensureTurnstile(function () {
        if (typeof turnstile !== 'undefined' && turnstile.render) {
          tsWidgetId = turnstile.render(tsContainer, {
            sitekey: tsSitekey
          });
        }
      });
    }

    submitBtn.onclick = function () {
      var authorName = nameInput.value.trim();
      var content = bodyTextarea.value.trim();
      if (!authorName || !content) return;
      submitBtn.disabled = true;

      var body = 'target_path=' + encodeURIComponent(path) +
                 '&author_name=' + encodeURIComponent(authorName) +
                 '&content=' + encodeURIComponent(content) +
                 '&parent_id=' + parentId;
      if (urlInput.value.trim()) {
        body += '&author_url=' + encodeURIComponent(urlInput.value.trim());
      }

      // Include Turnstile response if enabled
      if (tsSitekey) {
        var tsToken = '';
        if (tsWidgetId !== null && typeof turnstile !== 'undefined') {
          try { tsToken = turnstile.getResponse(tsWidgetId); } catch (e) {}
        }
        if (!tsToken) {
          // Fallback: look for the auto-rendered hidden input
          var tsInput = document.querySelector('input[name="cf-turnstile-response"]');
          if (tsInput) tsToken = tsInput.value;
        }
        if (tsToken) {
          body += '&cf-turnstile-response=' + encodeURIComponent(tsToken);
        }
      }

      fetch(origin + '/api/comment', {
        method: 'POST',
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        body: body
      })
      .then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json().catch(function () { return {}; });
      })
      .then(function (data) {
        if (tsWidgetId !== null && typeof turnstile !== 'undefined') {
          try { turnstile.remove(tsWidgetId); } catch (e) {}
        }
        form.remove();
        var pending = document.createElement('div');
        pending.style.cssText = 'font-size:12px;color:#888;padding:4px 0;margin-left:32px';
        pending.textContent = pendingText;
        commentEl.appendChild(pending);

        var token = data && data.delete_token;
        if (token && window.localStorage) {
          var key = 'zapiska_del_' + parentId + '_' + token.slice(0, 8);
          try { localStorage.setItem(key, token); } catch (e) {}
        }

        refreshComments();
      })
      .catch(function () {
        submitBtn.disabled = false;
        alert(submitText + ' failed. Please try again.');
      });
    };
  }

  function refreshComments() {
    fetch(apiUrl)
      .then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
      })
      .then(function (data) {
        renderTree(data.comments || [], data.total);
      })
      .catch(function () {});
  }

  function renderTree(comments, total) {
    target.innerHTML = '';

    if (!comments || comments.length === 0) {
      target.appendChild(renderEmpty());
      return;
    }

    if (!hideHeading) {
      var heading = document.createElement('h2');
      heading.className = 'nc-heading';
      heading.textContent = headingText.replace('%d', total || comments.length);
      target.appendChild(heading);
    }

    var roots = buildTree(comments);
    roots.forEach(function (node) {
      target.appendChild(renderNode(node));
    });
  }

  function renderEmpty() {
    var el = document.createElement('p');
    el.className = 'nc-empty';
    el.textContent = emptyText;
    return el;
  }

  function renderError() {
    var el = document.createElement('p');
    el.className = 'nc-error';
    el.textContent = errorText;
    return el;
  }

  // ── Bootstrap ──────────────────────────────────────────────

  fetch(apiUrl)
    .then(function (r) {
      if (!r.ok) throw new Error('HTTP ' + r.status);
      return r.json();
    })
    .then(function (data) {
      renderTree(data.comments, data.total);
    })
    .catch(function () {
      target.innerHTML = '';
      target.appendChild(renderError());
    });
})();
