// Minimal, dependency-free, SANITIZING markdown -> HTML renderer for release
// notes (SPEC s15 ChangelogModal). Release bodies come from the GitHub releases
// API / the manifest, which is trusted-ish but still UNTRUSTED enough that we
// must NEVER inject raw HTML into the webview (an attacker who controls a release
// body could otherwise script-inject). The strategy:
//
//   1. HTML-escape the ENTIRE input first (so any `<script>` / `<img onerror>` /
//      raw tag becomes inert text), THEN
//   2. apply a SMALL whitelist of markdown transforms on the already-escaped
//      text: headings, bold, italic, inline code, fenced/indented code, links
//      (with a sanitized href - only http(s)), unordered + ordered lists, and
//      paragraph / line breaks.
//
// Because every transform runs on escaped text and only emits a fixed set of
// tags we generate ourselves, no caller-supplied HTML ever reaches the DOM. The
// output is assigned via `v-html` to a container; only the tags below appear.

/** HTML-escape the five significant characters so no raw markup survives. */
function escapeHtml(input: string): string {
  return input
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Only allow http(s) links; anything else (javascript:, data:, etc.) is
 * dropped to plain text so a malicious href cannot execute. The `url` here is
 * already HTML-escaped, so we test against the escaped form. */
function safeHref(url: string): string | null {
  const trimmed = url.trim();
  // Allow only absolute http(s) URLs.
  if (/^https?:\/\//i.test(trimmed)) {
    return trimmed;
  }
  return null;
}

/** Apply inline transforms (code, bold, italic, links) to one already-escaped
 * line of text. Order: inline code first (so its contents are not further
 * transformed), then links, then bold, then italic. */
function renderInline(escaped: string): string {
  let out = escaped;

  // Inline code: `code` -> <code>code</code>. Greedy-safe (non-backtick run).
  out = out.replace(/`([^`]+)`/g, (_m, code) => `<code>${code}</code>`);

  // Links: [text](url). `text` + `url` are already escaped; href is sanitized.
  out = out.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_m, text, url) => {
    const href = safeHref(url);
    if (href === null) return text; // drop the link, keep the text
    return `<a href="${href}" target="_blank" rel="noopener noreferrer">${text}</a>`;
  });

  // Bold: **text** -> <strong>. Run before italic so `**x**` is not eaten by `*`.
  out = out.replace(/\*\*([^*]+)\*\*/g, (_m, t) => `<strong>${t}</strong>`);

  // Italic: *text* -> <em>.
  out = out.replace(/\*([^*]+)\*/g, (_m, t) => `<em>${t}</em>`);

  return out;
}

/**
 * Render a markdown release body to SANITIZED HTML. The input is fully
 * HTML-escaped before any transform, so the returned string only ever contains
 * the small whitelist of tags this function emits.
 */
export function sanitizeMarkdown(input: string): string {
  if (!input) return "";
  const escaped = escapeHtml(input);
  const lines = escaped.split(/\r?\n/);

  const html: string[] = [];
  let inUl = false;
  let inOl = false;
  let inCode = false;
  const para: string[] = [];

  const flushPara = () => {
    if (para.length > 0) {
      html.push(`<p>${para.join("<br>")}</p>`);
      para.length = 0;
    }
  };
  const closeLists = () => {
    if (inUl) {
      html.push("</ul>");
      inUl = false;
    }
    if (inOl) {
      html.push("</ol>");
      inOl = false;
    }
  };

  for (const rawLine of lines) {
    const line = rawLine;

    // Fenced code block toggling (``` lines). Inside a fence, lines are emitted
    // verbatim (already escaped) with no inline transforms.
    if (/^\s*```/.test(line)) {
      flushPara();
      closeLists();
      if (inCode) {
        html.push("</code></pre>");
        inCode = false;
      } else {
        html.push("<pre><code>");
        inCode = true;
      }
      continue;
    }
    if (inCode) {
      html.push(line);
      continue;
    }

    // Blank line: paragraph / list separator.
    if (line.trim() === "") {
      flushPara();
      closeLists();
      continue;
    }

    // Headings: # .. ###### -> <h1>..<h6>.
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      flushPara();
      closeLists();
      const level = heading[1].length;
      html.push(`<h${level}>${renderInline(heading[2])}</h${level}>`);
      continue;
    }

    // Unordered list item: -, *, or + followed by a space.
    const ul = /^\s*[-*+]\s+(.*)$/.exec(line);
    if (ul) {
      flushPara();
      if (inOl) {
        html.push("</ol>");
        inOl = false;
      }
      if (!inUl) {
        html.push("<ul>");
        inUl = true;
      }
      html.push(`<li>${renderInline(ul[1])}</li>`);
      continue;
    }

    // Ordered list item: 1. text.
    const ol = /^\s*\d+\.\s+(.*)$/.exec(line);
    if (ol) {
      flushPara();
      if (inUl) {
        html.push("</ul>");
        inUl = false;
      }
      if (!inOl) {
        html.push("<ol>");
        inOl = true;
      }
      html.push(`<li>${renderInline(ol[1])}</li>`);
      continue;
    }

    // Plain text line: accumulate into the current paragraph.
    closeLists();
    para.push(renderInline(line.trim()));
  }

  // Close anything left open.
  flushPara();
  closeLists();
  if (inCode) html.push("</code></pre>");

  return html.join("");
}
