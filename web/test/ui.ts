// The primitives a jsdom test renders, beside the React they render under, so the test drives the one copy of React
// the components were bundled with.
export { act, createElement } from 'react';
export { createRoot } from 'react-dom/client';
export { Menu } from '@/components/ui/primitives';
