/** The router's part of the documentation site: the `site_url` the site's configuration publishes, with the router's folder. */
const DOCS_SITE = 'https://phoebis-project.github.io/phoebis-docs/router/';

export type DocsPage = 'claude-ai-features' | 'installation';

/** A page of the documentation site, with a heading's anchor where one is named. */
export function docsHref(page: DocsPage, anchor?: string): string {
  return `${DOCS_SITE}${page}/${anchor === undefined ? '' : `#${anchor}`}`;
}
