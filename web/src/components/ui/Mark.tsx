import { type SVGProps, useId } from 'react';

type Props = SVGProps<SVGSVGElement> & { size?: number };

/**
 * Every fill, stroke and gradient stop reads a theme token in place of the mark files' hex values, so the theme toggle
 * reaches the whole mark. The favicon can only follow the browser's scheme. Each mark draws its gradients under ids of
 * its own, since two marks on one page would otherwise share an id, and a gradient inside a hidden mark paints nothing
 * for the other.
 */
export const Mark = ({ size = 20, ...rest }: Props) => {
  const id = useId();
  const lobe = (name: string) => `url(#${id}-${name})`;
  return (
    <svg viewBox="0 0 96 96" width={size} height={size} aria-hidden {...rest}>
      <defs>
        <linearGradient id={`${id}-a`} gradientUnits="userSpaceOnUse" x1="39.619" y1="47.612" x2="12.381" y2="76.946">
          <stop offset="0" stopColor="var(--mark-wing-2)" />
          <stop offset="1" stopColor="var(--mark-wing-3)" />
        </linearGradient>
        <linearGradient id={`${id}-b`} gradientUnits="userSpaceOnUse" x1="6.095" y1="21.422" x2="39.619" y2="54.946">
          <stop offset="0" stopColor="var(--mark-wing-1)" />
          <stop offset="1" stopColor="var(--mark-wing-2)" />
        </linearGradient>
        <linearGradient id={`${id}-c`} gradientUnits="userSpaceOnUse" x1="56.381" y1="47.612" x2="83.619" y2="76.946">
          <stop offset="0" stopColor="var(--mark-wing-2)" />
          <stop offset="1" stopColor="var(--mark-wing-3)" />
        </linearGradient>
        <linearGradient id={`${id}-d`} gradientUnits="userSpaceOnUse" x1="89.905" y1="21.422" x2="56.381" y2="54.946">
          <stop offset="0" stopColor="var(--mark-wing-1)" />
          <stop offset="1" stopColor="var(--mark-wing-2)" />
        </linearGradient>
      </defs>
      <path d="M39.619 47.612L24.952 47.612A14.667 14.667 0 1 0 39.619 62.279Z" fill={lobe('a')} />
      <path d="M39.619 54.946L39.619 37.136A17.81 17.81 0 1 0 21.81 54.946Z" fill={lobe('b')} />
      <path d="M56.381 47.612L56.381 62.279A14.667 14.667 0 1 0 71.048 47.612Z" fill={lobe('c')} />
      <path d="M56.381 54.946L74.19 54.946A17.81 17.81 0 1 0 56.381 37.136Z" fill={lobe('d')} />
      <path d="M39.619 47.612L24.952 47.612A14.667 14.667 0 0 0 13.63 52.956A17.81 17.81 0 0 0 21.81 54.946L39.619 54.946Z" fill="var(--mark-wing-shared)" />
      <path d="M56.381 47.612L56.381 54.946L74.19 54.946A17.81 17.81 0 0 0 82.37 52.956A14.667 14.667 0 0 0 71.048 47.612Z" fill="var(--mark-wing-shared)" />
      <path d="M46.324 28.755Q44.857 16.184 36.476 12.517" fill="none" stroke="var(--mark-ink)" strokeWidth={2.724} strokeLinecap="round" />
      <circle cx="36.476" cy="12.517" r="2.891" fill="var(--mark-ink)" />
      <path d="M49.676 28.755Q51.143 16.184 59.524 12.517" fill="none" stroke="var(--mark-ink)" strokeWidth={2.724} strokeLinecap="round" />
      <circle cx="59.524" cy="12.517" r="2.891" fill="var(--mark-ink)" />
      <rect x="44.333" y="24.565" width="7.333" height="61.81" rx="3.667" fill="var(--mark-ink)" />
    </svg>
  );
};
