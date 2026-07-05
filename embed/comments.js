/**
 * comments.js — Embeddable comment widget for zapiska
 *
 * Renders nested (threaded) comments with inline reply forms.
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
 * Threaded replies are indented, and each comment has a "Reply" button
 * that opens an inline form.
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

  // ── Minimal embedded styles for thread layout ───────────────
  // These are injected once to style the threaded comment tree.
  (function injectStyles() {
    if (document.getElementById('nc-thread-styles')) return;
    var css = document.createElement('style');
    css.id = 'nc-thread-styles';
    css.textContent =
      '.nc-comment { margin-bottom: 0; }' +
      '.nc-thread { margin-left: 32px; border-left: 1px solid #ccc; padding-left: 12px; }' +
      '.nc-meta { display: flex; align-items: center; gap: 6px; margin-bottom: 4px; font-size: 13px; }' +
      '.nc-avatar { width: 24px; height: 24px; border-radius: 50%; object-fit: cover; flex-shrink: 0; }' +
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

  // ── HTML sanitizer (defence-in-depth) ───────────────────────
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

  /**
   * Build a nested tree from a flat comment list.
   * Each node: { comment: {...}, children: [node, ...] }
   * Top-level comments (parent_id = null) are roots.
   * Replies are sorted oldest-first within their parent.
   */
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

    // Sort roots newest-first, replies oldest-first within each parent.
    roots.sort(function (a, b) { return b.comment.id - a.comment.id; });
    Object.keys(byId).forEach(function (id) {
      byId[id].children.sort(function (a, b) { return a.comment.id - b.comment.id; });
    });

    return roots;
  }

  // ── Rendering ──────────────────────────────────────────────

  /**
   * Render a single comment element (without children).
   * Sets data-id for later DOM lookups.
   */
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

    // Meta row: avatar, author, date
    var meta = document.createElement('div');
    meta.className = 'nc-meta';

    if (avatar) {
      var img = document.createElement('img');
      img.className = 'nc-avatar';
      img.src = avatar;
      img.alt = name;
      img.width = 24;
      img.height = 24;
      img.loading = 'lazy';
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

    var time = document.createElement('time');
    time.className = 'nc-date';
    time.textContent = date;
    meta.appendChild(time);

    el.appendChild(meta);

    // Body
    var body = document.createElement('div');
    body.className = 'nc-body';
    body.innerHTML = content;
    el.appendChild(body);

    // Reply button
    var replyBtn = document.createElement('button');
    replyBtn.className = 'nc-reply-btn';
    replyBtn.textContent = 'Reply';
    replyBtn.onclick = function () { toggleReplyForm(el, c.id, c.depth); };
    el.appendChild(replyBtn);

    return el;
  }

  /**
   * Recursively render a comment node and its children.
   * Children are wrapped in a .nc-thread container for indentation.
   */
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

  /**
   * Toggle an inline reply form below a comment.
   * Only one form can be open at a time (closes any existing one).
   */
  function toggleReplyForm(commentEl, parentId, parentDepth) {
    var existing = commentEl.querySelector('.nc-reply-form');
    if (existing) {
      existing.remove();
      return;
    }

    // Close any other open reply forms
    document.querySelectorAll('.nc-reply-form').forEach(function (f) { f.remove(); });

    var form = document.createElement('div');
    form.className = 'nc-reply-form';

    var nameInput = document.createElement('input');
    nameInput.type = 'text';
    nameInput.placeholder = 'Your name';
    nameInput.required = true;

    var urlInput = document.createElement('input');
    urlInput.type = 'url';
    urlInput.placeholder = 'Website (optional)';

    // Honeypot field — hidden from humans, auto-filled by bots.
    // If non-empty, the server silently discards the submission.
    var honeypot = document.createElement('input');
    honeypot.type = 'text';
    honeypot.name = 'website';
    honeypot.tabIndex = -1;
    honeypot.autocomplete = 'off';
    honeypot.style.cssText = 'position:absolute;left:-9999px;top:-9999px;width:1px;height:1px;opacity:0';

    var bodyTextarea = document.createElement('textarea');
    bodyTextarea.placeholder = 'Write your reply...';
    bodyTextarea.required = true;

    var actions = document.createElement('div');
    actions.className = 'nc-form-actions';

    var submitBtn = document.createElement('button');
    submitBtn.className = 'nc-submit';
    submitBtn.textContent = 'Submit';

    var cancelBtn = document.createElement('button');
    cancelBtn.textContent = 'Cancel';
    cancelBtn.onclick = function () { form.remove(); };

    actions.appendChild(submitBtn);
    actions.appendChild(cancelBtn);

    form.appendChild(nameInput);
    form.appendChild(urlInput);
    form.appendChild(honeypot);
    form.appendChild(bodyTextarea);
    form.appendChild(actions);

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

      fetch(apiOrigin + '/api/comment', {
        method: 'POST',
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        body: body
      })
      .then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json().catch(function () { return {}; });
      })
      .then(function (data) {
        // Clear form
        form.remove();
        // Show pending message with optional self-delete link
        var pending = document.createElement('div');
        pending.style.cssText = 'font-size:12px;color:#888;padding:4px 0;margin-left:32px';
        pending.textContent = 'Reply submitted (pending approval).';
        commentEl.appendChild(pending);

        // If a delete token was returned, show a delete link
        var token = data && data.delete_token;
        if (token && window.localStorage) {
          // Store token for potential later use
          var key = 'zapiska_del_' + parentId + '_' + token.slice(0, 8);
          try { localStorage.setItem(key, token); } catch (e) {}
        }

        // Re-fetch the full list to get the new comment in the tree
        refreshComments();
      })
      .catch(function () {
        submitBtn.disabled = false;
        alert('Failed to submit reply. Please try again.');
      });
    };

    commentEl.appendChild(form);
    nameInput.focus();
  }

  /**
   * Re-fetch all comments and re-render the full tree.
   * This is called after a new reply is submitted so it appears in the thread.
   */
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

  /**
   * Render the full comment tree into the target element.
   */
  function renderTree(comments, total) {
    target.innerHTML = '';

    if (!comments || comments.length === 0) {
      target.appendChild(renderEmpty());
      return;
    }

    var heading = document.createElement('h2');
    heading.className = 'nc-heading';
    heading.textContent = 'Comments (' + (total || comments.length) + ')';
    target.appendChild(heading);

    var roots = buildTree(comments);
    roots.forEach(function (node) {
      target.appendChild(renderNode(node));
    });
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
      renderTree(data.comments, data.total);
    })
    .catch(function () {
      target.innerHTML = '';
      target.appendChild(renderError());
    });
})();
