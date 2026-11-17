/** The documentation site's address, the `site_url` its configuration publishes each page under. */
const DOCS_SITE = 'https://phoebis-project.github.io/phoebis/';

export type DocsPage = 'claude-ai-features' | 'installation';

/** A page of the documentation site, with a heading's anchor where one is named. */
export function docsHref(page: DocsPage, anchor?: string): string {
  return `${DOCS_SITE}${page}/${anchor === undefined ? '' : `#${anchor}`}`;
}
