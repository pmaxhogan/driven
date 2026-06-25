// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { mount } from "@vue/test-utils";

import { i18n } from "../i18n";
import ChangelogModal from "../components/ChangelogModal.vue";
import { sanitizeMarkdown } from "../components/sanitizeMarkdown";
import type { ReleaseDto } from "../ipc/types";

// ChangelogModal tests (SPEC s15 / ROADMAP M9). The modal renders a release's
// markdown notes through `sanitizeMarkdown`, which HTML-escapes first then emits
// only a whitelist of tags - so a malicious release body can never inject raw
// HTML / scripts. These tests assert the modal renders a sample body AND that the
// sanitizer is XSS-safe.

function release(notes: string): ReleaseDto {
  return {
    version: "0.2.0",
    name: "Driven 0.2.0",
    notes,
    publishedAt: "2026-06-24T00:00:00Z",
    url: "https://github.com/pmaxhogan/driven/releases/0.2.0",
  };
}

function mountModal(rel: ReleaseDto | null) {
  return mount(ChangelogModal, {
    props: { release: rel },
    global: { plugins: [i18n] },
  });
}

describe("ChangelogModal", () => {
  it("is hidden when no release is provided", () => {
    const wrapper = mountModal(null);
    expect(wrapper.find('[data-testid="changelog-modal"]').exists()).toBe(false);
  });

  it("renders a sample release body as formatted HTML", () => {
    const wrapper = mountModal(
      release("## Highlights\n\n- Faster **sync**\n- Fixed `crash`\n"),
    );
    expect(wrapper.find('[data-testid="changelog-modal"]').exists()).toBe(true);
    const body = wrapper.find('[data-testid="changelog-body"]');
    expect(body.exists()).toBe(true);
    const html = body.html();
    // Markdown was rendered into the whitelist tags.
    expect(html).toContain("<h2>Highlights</h2>");
    expect(html).toContain("<strong>sync</strong>");
    expect(html).toContain("<code>crash</code>");
    expect(html).toContain("<li>");
  });

  it("shows a no-notes message for an empty body", () => {
    const wrapper = mountModal(release(""));
    expect(wrapper.find('[data-testid="changelog-empty"]').exists()).toBe(true);
    expect(wrapper.text()).toContain(i18n.global.t("changelog.noNotes"));
  });

  it("emits close when the close button is clicked", async () => {
    const wrapper = mountModal(release("notes"));
    const closeBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.close"));
    expect(closeBtn).toBeTruthy();
    await closeBtn!.trigger("click");
    expect(wrapper.emitted("close")).toBeTruthy();
  });
});

describe("sanitizeMarkdown XSS safety", () => {
  it("escapes raw HTML / script tags so they are inert text", () => {
    const out = sanitizeMarkdown(
      "Hello <script>alert('x')</script> <img src=x onerror=alert(1)>",
    );
    // No live tags: the angle brackets are escaped.
    expect(out).not.toContain("<script>");
    expect(out).not.toContain("<img");
    expect(out).toContain("&lt;script&gt;");
    expect(out).toContain("&lt;img");
  });

  it("drops non-http(s) link hrefs but keeps the link text", () => {
    const out = sanitizeMarkdown("[click](javascript:alert(1))");
    expect(out).not.toContain("javascript:");
    // The text survives as plain text.
    expect(out).toContain("click");
  });

  it("keeps http(s) links with safe rel/target", () => {
    const out = sanitizeMarkdown("[docs](https://driven.maxhogan.dev)");
    expect(out).toContain('href="https://driven.maxhogan.dev"');
    expect(out).toContain('rel="noopener noreferrer"');
  });
});
