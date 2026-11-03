import type { SVGProps } from 'react';

type Props = SVGProps<SVGSVGElement> & { size?: number };

const base = ({ size = 16, ...rest }: Props) => ({
  fill: 'none',
  height: size,
  stroke: 'currentColor',
  strokeLinecap: 'round' as const,
  strokeLinejoin: 'round' as const,
  strokeWidth: 1.75,
  viewBox: '0 0 24 24',
  width: size,
  'aria-hidden': true,
  ...rest,
});

export const IconGrid = (p: Props) => (
  <svg {...base(p)}>
    <rect x="3" y="3" width="7" height="7" rx="1.5" />
    <rect x="14" y="3" width="7" height="7" rx="1.5" />
    <rect x="3" y="14" width="7" height="7" rx="1.5" />
    <rect x="14" y="14" width="7" height="7" rx="1.5" />
  </svg>
);
export const IconGauge = (p: Props) => (
  <svg {...base(p)}>
    <path d="M4 15a8 8 0 1 1 16 0" />
    <path d="M12 15l4-5" />
    <circle cx="12" cy="15" r="1.2" fill="currentColor" />
  </svg>
);
export const IconUser = (p: Props) => (
  <svg {...base(p)}>
    <circle cx="12" cy="8" r="4" />
    <path d="M4 21a8 8 0 0 1 16 0" />
  </svg>
);
export const IconLayers = (p: Props) => (
  <svg {...base(p)}>
    <path d="M12 3 3 8l9 5 9-5-9-5Z" />
    <path d="m3 13 9 5 9-5" />
  </svg>
);
export const IconKey = (p: Props) => (
  <svg {...base(p)}>
    <circle cx="8" cy="14" r="4" />
    <path d="M11 11 20 2M15 7l3 3M18 4l2 2" />
  </svg>
);
export const IconList = (p: Props) => (
  <svg {...base(p)}>
    <path d="M8 6h13M8 12h13M8 18h13" />
    <circle cx="3.5" cy="6" r="0.9" fill="currentColor" />
    <circle cx="3.5" cy="12" r="0.9" fill="currentColor" />
    <circle cx="3.5" cy="18" r="0.9" fill="currentColor" />
  </svg>
);
export const IconChart = (p: Props) => (
  <svg {...base(p)}>
    <path d="M3 21h18" />
    <path d="M6 17v-6M11 17V7M16 17v-3M21 17V4" />
  </svg>
);
export const IconSessions = (p: Props) => (
  <svg {...base(p)}>
    <path d="M8 7V5.5A1.5 1.5 0 0 1 9.5 4h10A1.5 1.5 0 0 1 21 5.5v9a1.5 1.5 0 0 1-1.5 1.5H18" />
    <rect x="3" y="8" width="13" height="12" rx="1.5" />
    <path d="M3 12h13" />
  </svg>
);
export const IconRoute = (p: Props) => (
  <svg {...base(p)}>
    <circle cx="6" cy="19" r="2" />
    <circle cx="18" cy="5" r="2" />
    <circle cx="18" cy="17" r="2" />
    <path d="M6 17V10a5 5 0 0 1 5-5h5" />
    <path d="M6 12a5 5 0 0 0 5 5h5" />
  </svg>
);
export const IconActivity = (p: Props) => (
  <svg {...base(p)}>
    <path d="M3 12h4l3-8 4 16 3-8h4" />
  </svg>
);
export const IconSliders = (p: Props) => (
  <svg {...base(p)}>
    <path d="M4 7h10M18 7h2M4 17h4M12 17h8" />
    <circle cx="16" cy="7" r="2" />
    <circle cx="10" cy="17" r="2" />
  </svg>
);
export const IconSun = (p: Props) => (
  <svg {...base(p)}>
    <circle cx="12" cy="12" r="4" />
    <path d="M12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4" />
  </svg>
);
export const IconMoon = (p: Props) => (
  <svg {...base(p)}>
    <path d="M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5Z" />
  </svg>
);
export const IconMonitor = (p: Props) => (
  <svg {...base(p)}>
    <rect x="3" y="4" width="18" height="12" rx="2" />
    <path d="M8 20h8M12 16v4" />
  </svg>
);
export const IconSearch = (p: Props) => (
  <svg {...base(p)}>
    <circle cx="11" cy="11" r="6" />
    <path d="m20 20-4.5-4.5" />
  </svg>
);
export const IconChevron = (p: Props) => (
  <svg {...base(p)}>
    <path d="m6 9 6 6 6-6" />
  </svg>
);
export const IconChevronLeft = (p: Props) => (
  <svg {...base(p)}>
    <path d="m15 6-6 6 6 6" />
  </svg>
);
export const IconChevronRight = (p: Props) => (
  <svg {...base(p)}>
    <path d="m9 6 6 6-6 6" />
  </svg>
);
export const IconChevronsLeft = (p: Props) => (
  <svg {...base(p)}>
    <path d="m11 6-6 6 6 6M18 6l-6 6 6 6" />
  </svg>
);
export const IconX = (p: Props) => (
  <svg {...base(p)}>
    <path d="M6 6l12 12M18 6 6 18" />
  </svg>
);
export const IconCopy = (p: Props) => (
  <svg {...base(p)}>
    <rect x="9" y="9" width="12" height="12" rx="2" />
    <path d="M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1" />
  </svg>
);
export const IconCheck = (p: Props) => (
  <svg {...base(p)}>
    <path d="m5 12 4.5 4.5L19 7" />
  </svg>
);
export const IconRefresh = (p: Props) => (
  <svg {...base(p)}>
    <path d="M20 12a8 8 0 1 1-2.3-5.7" />
    <path d="M20 4v5h-5" />
  </svg>
);
export const IconPlus = (p: Props) => (
  <svg {...base(p)}>
    <path d="M12 5v14M5 12h14" />
  </svg>
);
export const IconPencil = (p: Props) => (
  <svg {...base(p)}>
    <path d="m4 20 4-1L19 8l-3-3L5 16l-1 4Z" />
    <path d="m14 7 3 3" />
  </svg>
);
export const IconWarning = (p: Props) => (
  <svg {...base(p)}>
    <path d="M12 3 2 21h20L12 3Z" />
    <path d="M12 10v5M12 18v.5" />
  </svg>
);
export const IconInfo = (p: Props) => (
  <svg {...base(p)}>
    <circle cx="12" cy="12" r="9" />
    <path d="M12 11v6M12 7v.5" />
  </svg>
);
export const IconArrowRight = (p: Props) => (
  <svg {...base(p)}>
    <path d="M5 12h14M13 6l6 6-6 6" />
  </svg>
);
export const IconColumns = (p: Props) => (
  <svg {...base(p)}>
    <rect x="3" y="4" width="18" height="16" rx="2" />
    <path d="M9 4v16M15 4v16" />
  </svg>
);
export const IconFunnel = (p: Props) => (
  <svg {...base(p)}>
    <path d="M3 4h18l-7 8.5V19l-4 2v-8.5L3 4Z" />
  </svg>
);
