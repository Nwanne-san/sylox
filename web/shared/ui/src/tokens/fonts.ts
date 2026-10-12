import { Inter, Space_Mono, Big_Shoulders } from "next/font/google";

// Only the weights actually used anywhere on the page (brief §4.4, §5.5 —
// Proofbridge shipped 178KB of a font nobody used; audit this list against
// the final bundle before shipping).

export const inter = Inter({
  subsets: ["latin"],
  weight: ["300", "400", "500", "600"],
  variable: "--font-inter",
  display: "swap",
});

export const spaceMono = Space_Mono({
  subsets: ["latin"],
  weight: ["400", "700"],
  variable: "--font-space-mono",
  display: "swap",
});

// Display stand-in for the brand's hand-drawn wordmark (brief §4.4). Swap
// for a licensed face later by changing this import and keeping the same
// --font-display-family token.
export const bigShoulders = Big_Shoulders({
  subsets: ["latin"],
  weight: ["700", "800", "900"],
  variable: "--font-display-family",
  display: "swap",
  adjustFontFallback: false,
});
