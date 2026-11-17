import type { ReactElement } from 'react';
import { Rectangle, type RectangleProps } from 'recharts';

/** The narrowest bar the shape draws. A bar of a fraction of a pixel reads as a faint line or as nothing. */
const MIN_WIDTH = 1;
/** The narrowest bar that draws its outline. The outline covers half a pixel inside each edge. */
const OUTLINE_MIN_WIDTH = 4;

/**
 * A bar of a time chart, handed to each `Bar` as its `shape`. A bar narrower than a pixel draws one whole pixel around
 * its own center, so two neighbors of a dense bucket can overlap by a part of a pixel. A stacked `Bar` sets the outline
 * that parts its segments as its `stroke`, and a bar under the outline's floor draws without it, so that it keeps its
 * fill. A bar with no height draws nothing, since `Rectangle` draws none.
 */
export const chartBar = (handed: unknown): ReactElement => {
  // Recharts hands a shape the Bar's props with the bucket's x, y, width and height, which its type offers as unknown.
  const props = handed as RectangleProps;
  const width = props.width ?? 0;
  const drawn = Math.max(width, MIN_WIDTH);
  return <Rectangle {...props} x={(props.x ?? 0) - (drawn - width) / 2} width={drawn} stroke={drawn >= OUTLINE_MIN_WIDTH ? props.stroke : 'none'} />;
};

/**
 * The props every bar of a time chart shares, spread on its `Bar` beside its own data key, fill and width cap. Recharts
 * finds a chart's series among its children by element type, so a component wrapping `Bar` would draw nothing.
 */
export const TIME_BAR: { isAnimationActive: boolean; radius: [number, number, number, number]; shape: typeof chartBar } = { isAnimationActive: false, radius: [3, 3, 0, 0], shape: chartBar };
