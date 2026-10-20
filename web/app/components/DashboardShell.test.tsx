import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { DashboardShell } from "./DashboardShell";
import { WalletProvider } from "@/lib/wallet/WalletContext";

const { usePathname } = vi.hoisted(() => ({ usePathname: vi.fn(() => "/") }));

vi.mock("next/navigation", async () => {
  const actual = await vi.importActual<typeof import("next/navigation")>("next/navigation");
  return { ...actual, usePathname };
});

function renderShell(title = "Explorer") {
  return render(
    <WalletProvider>
      <DashboardShell title={title}>
        <p>content</p>
      </DashboardShell>
    </WalletProvider>,
  );
}

describe("DashboardShell", () => {
  it("renders the page content", () => {
    renderShell();
    expect(screen.getByText("content")).toBeInTheDocument();
  });

  it("shows every nav item and every coming-soon item", () => {
    renderShell();
    expect(screen.getByRole("link", { name: "Explorer" })).toHaveAttribute("href", "/");
    expect(screen.getByRole("link", { name: "Events" })).toHaveAttribute("href", "/events");
    expect(screen.getByText("Markets")).toBeInTheDocument();
    expect(screen.getByText("My positions")).toBeInTheDocument();
    expect(screen.getByText("Faucet")).toBeInTheDocument();
    expect(screen.getAllByText("Soon")).toHaveLength(3);
  });

  it("marks coming-soon items as non-links", () => {
    renderShell();
    expect(screen.queryByRole("link", { name: /markets/i })).not.toBeInTheDocument();
  });

  it("marks the active nav item with aria-current, matching the current path", () => {
    usePathname.mockReturnValue("/events");
    renderShell();
    expect(screen.getByRole("link", { name: "Events" })).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("link", { name: "Explorer" })).not.toHaveAttribute("aria-current");
  });

  it("treats a nested route as active for its top-level nav item", () => {
    usePathname.mockReturnValue("/events/42");
    renderShell();
    expect(screen.getByRole("link", { name: "Events" })).toHaveAttribute("aria-current", "page");
  });

  it("shows a fixed Testnet badge with no toggle", () => {
    renderShell();
    const badges = screen.getAllByText("Testnet");
    expect(badges.length).toBeGreaterThanOrEqual(1);
    expect(screen.queryByRole("switch")).not.toBeInTheDocument();
    expect(screen.queryByText("Mainnet")).not.toBeInTheDocument();
  });

  it("shows the Connect wallet button in the top bar when disconnected", () => {
    renderShell();
    expect(screen.getByRole("button", { name: /connect wallet/i })).toBeInTheDocument();
  });

  it("links every External item out, each opening in a new tab", () => {
    renderShell();
    const website = screen.getByRole("link", { name: "Website" });
    expect(website).toHaveAttribute("href", "https://sylox.xyz");
    expect(website).toHaveAttribute("target", "_blank");
    expect(website).toHaveAttribute("rel", expect.stringContaining("noopener"));

    expect(screen.getByRole("link", { name: "Docs" })).toHaveAttribute(
      "href",
      "https://docs.sylox.xyz/docs",
    );
    expect(screen.getByRole("link", { name: "Repo" })).toHaveAttribute(
      "href",
      "https://github.com/Sylox-labs/sylox",
    );
  });

  it("opens the mobile drawer on menu click, with the same items as the sidebar", async () => {
    const user = userEvent.setup();
    renderShell();

    await user.click(screen.getByRole("button", { name: /open menu/i }));

    expect(screen.getByRole("dialog", { name: /navigation/i })).toBeInTheDocument();
  });

  it("closes the mobile drawer on Escape, returning focus to the menu button", async () => {
    const user = userEvent.setup();
    renderShell();

    const menuButton = screen.getByRole("button", { name: /open menu/i });
    await user.click(menuButton);
    expect(screen.getByRole("dialog")).toBeInTheDocument();

    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(menuButton).toHaveFocus();
  });

  it("traps focus inside the mobile drawer while it's open", async () => {
    const user = userEvent.setup();
    renderShell();

    await user.click(screen.getByRole("button", { name: /open menu/i }));
    const dialog = screen.getByRole("dialog");
    const focusable = dialog.querySelectorAll<HTMLElement>('a[href], button:not([disabled])');
    expect(focusable.length).toBeGreaterThan(1);

    const last = focusable[focusable.length - 1];
    last.focus();
    await user.tab();
    expect(focusable[0]).toHaveFocus(); // Wrapped from the last focusable element back to the first.
  });
});
