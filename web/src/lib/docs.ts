/**
 * The router's part of the documentation site. The address follows `site_url` in `zensical.toml` of the `phoebis-docs`
 * repository, with the router's folder behind it, so a change of that value is followed here by hand.
 */
const DOCS_SITE = 'https://phoebis-project.github.io/phoebis-docs/router/';

export type DocsPage = 'claude-ai-features' | 'installation';

/** A page of the documentation site, with a heading's anchor where one is named. */
export function docsHref(page: DocsPage, anchor?: string): string {
  return `${DOCS_SITE}${page}/${anchor === undefined ? '' : `#${anchor}`}`;
}
