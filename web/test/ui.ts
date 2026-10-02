// The primitives a jsdom test renders, beside the React they render under, so the test drives the one copy of React
// the components were bundled with.
export { act, createElement, useEffect, useState } from 'react';
export { createRoot } from 'react-dom/client';
export { Menu } from '@/components/ui/primitives';
export { installClock } from '@/lib/clock';
export { useAsync, useNow } from '@/lib/hooks';
export { fakeClock } from '../smoke/clock';
