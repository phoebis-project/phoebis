import { createApiReference } from '@scalar/api-reference';
import '@scalar/api-reference/style.css';

// Every switch keeps the page on its own origin: the document is a path on it, the default fonts would come from
// a font host, and no proxy is named, so a try-it-out call goes from the browser to the admin listener. The
// bearer scheme is selected so the page opens with the Authorization input shown, the typed secret stays in
// memory (persistAuth at its default), and the theme is Scalar's own, named so a reader sees the choice.
createApiReference('#app', {
  url: '/api-reference/openapi.json',
  withDefaultFonts: false,
  authentication: { preferredSecurityScheme: 'bearer' },
  theme: 'default',
  // The agent chat, the MCP generator, the developer tools and the hosted client each lead to a Scalar service.
  agent: { disabled: true },
  hideClientButton: true,
  mcp: { disabled: true },
  showDeveloperTools: 'never',
  // The document's server is `/`, which the client's send label joins to a path as `//`.
  servers: [{ url: window.location.origin }],
});
