"use client";

import { useEffect, useRef, useState } from "react";
import Image from "next/image";
import Link from "next/link";
import { usePathname } from "next/navigation";
import { WalletButton } from "./WalletButton";

interface NavItem {
  label: string;
  href: string;
}

const NAV_ITEMS: NavItem[] = [
  { label: "Explorer", href: "/" },
  { label: "Events", href: "/events" },
];

const COMING_SOON_ITEMS = ["Markets", "My positions", "Faucet"];

const EXTERNAL_LINKS: NavItem[] = [
  { label: "Website", href: "https://sylox.xyz" },
  { label: "Docs", href: "https://docs.sylox.xyz/docs" },
  { label: "Repo", href: "https://github.com/Sylox-labs/sylox" },
];

function isActive(pathname: string | null, href: string): boolean {
  if (pathname === null) return false;
  if (href === "/") return pathname === "/";
  return pathname === href || pathname.startsWith(`${href}/`);
}

function NavLink({ item, active }: { item: NavItem; active: boolean }) {
  return (
    <Link
      href={item.href}
      aria-current={active ? "page" : undefined}
      className={`relative flex items-center gap-3 px-4 py-2 pl-5 font-mono text-sm transition-colors ${
        active ? "text-silo-oatmeal" : "text-cyber-tin hover:text-silo-oatmeal"
      }`}
    >
      {/* A thin marker, not a filled block (brief: "crimson as a thin marker only"). */}
      <span
        className={`absolute left-0 top-1/2 h-4 w-0.5 -translate-y-1/2 bg-risk-crimson transition-opacity ${
          active ? "opacity-100" : "opacity-0"
        }`}
        aria-hidden="true"
      />
      {item.label}
    </Link>
  );
}

function ComingSoonItem({ label }: { label: string }) {
  return (
    <div className="flex items-center justify-between px-4 py-2 pl-5 font-mono text-sm text-cyber-tin/50">
      <span>{label}</span>
      <span className="rounded-full border border-cement-grey/30 px-2 py-0.5 font-mono text-[9px] uppercase tracking-wide text-cyber-tin/60">
        Soon
      </span>
    </div>
  );
}

function SidebarContent({ pathname }: { pathname: string | null }) {
  return (
    <div className="flex h-full flex-col">
      <Link href="/" className="flex w-full items-center justify-center gap-3 px-4 py-6" aria-label="Sylox home">
        {/* width/height set the image's own aspect ratio (1315:1139,
            the artwork's real proportions after trimming its
            transparent margin - see public/brand/sylox-mark.webp's
            own history); h-10 w-auto sizes by height only and lets
            width follow that ratio, so a future swap of the source
            art can't silently squash it the way a mismatched fixed
            box (previously 32x14, a different ratio entirely) did. */}
        <Image src="/brand/sylox-mark.webp" alt="" width={1315} height={1139} className="h-10 w-auto" priority aria-hidden="true" />
        <span className="font-display text-2xl tracking-tight text-silo-oatmeal">Sylox</span>
      </Link>

      <nav className="mt-4 flex flex-col gap-1" aria-label="Primary">
        <p className="px-5 font-mono text-[10px] uppercase tracking-[0.15em] text-cyber-tin/60">Navigation</p>
        <div className="mt-2 flex flex-col gap-1">
          {NAV_ITEMS.map((item) => (
            <NavLink key={item.href} item={item} active={isActive(pathname, item.href)} />
          ))}
          {COMING_SOON_ITEMS.map((label) => (
            <ComingSoonItem key={label} label={label} />
          ))}
        </div>
      </nav>

      <div className="mt-auto flex flex-col gap-1 pb-6">
        <p className="px-5 font-mono text-[10px] uppercase tracking-[0.15em] text-cyber-tin/60">External</p>
        <div className="mt-2 flex flex-col gap-1">
          {EXTERNAL_LINKS.map((item) => (
            <a
              key={item.href}
              href={item.href}
              target="_blank"
              rel="noopener noreferrer"
              className="px-5 py-2 font-mono text-sm text-cyber-tin transition-colors hover:text-silo-oatmeal"
            >
              {item.label}
            </a>
          ))}
        </div>
      </div>
    </div>
  );
}

const FOCUSABLE_SELECTOR = 'a[href], button:not([disabled])';

function MobileDrawer({ isOpen, onClose, pathname }: { isOpen: boolean; onClose: () => void; pathname: string | null }) {
  const drawerRef = useRef<HTMLDivElement>(null);
  const previouslyFocused = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!isOpen) return;
    previouslyFocused.current = document.activeElement as HTMLElement | null;

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        onClose();
        return;
      }
      if (event.key !== "Tab") return;

      const focusable = drawerRef.current?.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR);
      if (!focusable || focusable.length === 0) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];

      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };

    document.addEventListener("keydown", handleKeyDown);
    const firstFocusable = drawerRef.current?.querySelector<HTMLElement>(FOCUSABLE_SELECTOR);
    firstFocusable?.focus();

    return () => {
      document.removeEventListener("keydown", handleKeyDown);
      previouslyFocused.current?.focus();
    };
  }, [isOpen, onClose]);

  if (!isOpen) return null;

  return (
    <div className="fixed inset-0 z-50 flex md:hidden" onClick={onClose}>
      <div className="absolute inset-0 bg-slate-black/80 backdrop-blur-sm" aria-hidden="true" />
      <div
        ref={drawerRef}
        role="dialog"
        aria-modal="true"
        aria-label="Navigation"
        className="relative flex h-full w-72 flex-col border-r border-cement-grey/20 bg-slate-black"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex items-center justify-end px-4 pt-4">
          <button
            type="button"
            onClick={onClose}
            aria-label="Close menu"
            className="font-mono text-xs text-cyber-tin transition-colors hover:text-silo-oatmeal"
          >
            ESC
          </button>
        </div>
        <SidebarContent pathname={pathname} />
      </div>
    </div>
  );
}

/**
 * The dashboard shell: a persistent sidebar + top bar wrapping every
 * screen, so Explorer/Events/Asset/Event can be navigated to instead
 * of only reached by typing a URL directly. Read-only screens
 * (Explorer, Asset, Event, Events) stay fully readable without a
 * wallet - Connect Wallet lives in the top bar for when an action
 * needs one, not as a gate in front of this shell.
 */
export function DashboardShell({ title, children }: { title: string; children: React.ReactNode }) {
  const pathname = usePathname();
  const [isDrawerOpen, setIsDrawerOpen] = useState(false);

  return (
    <div className="flex min-h-screen">
      <aside className="sticky top-0 hidden h-screen w-60 shrink-0 overflow-y-auto border-r border-cement-grey/20 md:block">
        <SidebarContent pathname={pathname} />
      </aside>

      <MobileDrawer isOpen={isDrawerOpen} onClose={() => setIsDrawerOpen(false)} pathname={pathname} />

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-16 shrink-0 items-center justify-between gap-4 border-b border-cement-grey/20 px-4 md:px-8">
          <div className="flex min-w-0 items-center gap-3">
            <button
              type="button"
              onClick={() => setIsDrawerOpen(true)}
              aria-label="Open menu"
              className="shrink-0 font-mono text-xs text-cyber-tin transition-colors hover:text-silo-oatmeal md:hidden"
            >
              MENU
            </button>
            {/* The top bar's own label, not the page's real heading - each
                screen's content still owns its own single <h1> (e.g. the
                asset code on the Asset screen), so this is a <p>, not a
                second competing <h1> on the same page. */}
            <p className="truncate font-display text-lg tracking-tight text-silo-oatmeal">{title}</p>
          </div>

          <div className="flex shrink-0 items-center gap-3">
            <span className="rounded-full border border-risk-crimson/40 bg-risk-crimson/10 px-3 py-1 font-mono text-[10px] uppercase tracking-wide text-risk-crimson-tint">
              Testnet
            </span>
            <WalletButton />
          </div>
        </header>

        <div className="flex-1">{children}</div>
      </div>
    </div>
  );
}
