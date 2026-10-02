import type { SVGProps } from 'react';

type Props = SVGProps<SVGSVGElement> & { size?: number };

/**
 * The fills read theme tokens rather than the mark files' hex values, so the mark follows the console's theme toggle;
 * the favicon can only follow the browser's scheme.
 */
const CELLS: [number, number][] = [
  [18, 6], [66, 6], [30, 18], [54, 18], [6, 30], [18, 30], [66, 30], [78, 30], [6, 42], [18, 42], [30, 42], [54, 42],
  [66, 42], [78, 42], [6, 54], [18, 54], [30, 54], [54, 54], [66, 54], [78, 54], [18, 66], [30, 66], [54, 66], [66, 66],
  [30, 78], [54, 78],
];
const BODY: [number, number][] = [[42, 30], [42, 42], [42, 54], [42, 66], [42, 78]];

export const Mark = ({ size = 20, ...rest }: Props) => (
  <svg viewBox="0 0 96 96" width={size} height={size} aria-hidden {...rest}>
    {CELLS.map(([x, y]) => <rect key={`${x}-${y}`} x={x} y={y} width={10} height={10} rx={2.5} fill="var(--mark-cell)" />)}
    {BODY.map(([x, y]) => <rect key={`body-${y}`} x={x} y={y} width={10} height={10} rx={2.5} fill="var(--mark-body)" />)}
  </svg>
);
