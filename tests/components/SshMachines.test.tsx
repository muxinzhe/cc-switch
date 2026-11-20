import { useState } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { invoke } from "@tauri-apps/api/core";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { ProviderMachineFrame } from "@/ssh/ProviderMachineFrame";
import {
  RemoteConfigButton,
  RemoteConfigEditor,
  type RemoteConfigSelection,
} from "@/ssh/RemoteConfigEditor";
import { SshHosts } from "@/components/shell/SshHosts";
import { useSidebarStatus } from "@/hooks/useSidebarStatus";
import { useUsageSummaryByApp } from "@/lib/query/usage";
import { useMachineScope } from "@/ssh/MachineScope";
import {
  UsageMachineProvider,
  useUsageMachineSource,
} from "@/ssh/UsageMachine";
import type { SshHost } from "@/ssh/api";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@/components/JsonEditor", () => ({
  default: ({
    ariaLabel,
    value,
    onChange,
  }: {
    ariaLabel: string;
    value: string;
    onChange: (value: string) => void;
  }) => (
    <textarea
      aria-label={ariaLabel}
      value={value}
      onChange={(event) => onChange(event.target.value)}
    />
  ),
}));

beforeAll(() => {
  Element.prototype.scrollIntoView ??= () => {};
});

const hosts: SshHost[] = [
  { id: "server-a", name: "Machine A", destination: "work", enabled: true },
  { id: "server-b", name: "Machine B", destination: "test", enabled: true },
  {
    id: "server-c",
    name: "Disabled machine",
    destination: "staging",
    enabled: false,
  },
];

function renderPage(node: React.ReactNode) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>{node}</QueryClientProvider>,
  );
}

function SidebarToday() {
  const { todayCost } = useSidebarStatus(true);
  return <output aria-label="Sidebar today">{todayCost.toFixed(2)}</output>;
}

function DashboardToday() {
  const { data } = useUsageSummaryByApp({ preset: "today" });
  return (
    <output aria-label="Dashboard today">{data?.[0]?.summary.totalCost}</output>
  );
}

function DashboardSnapshot() {
  const machines = useMachineScope();
  const source = useUsageMachineSource(machines, false, {
    forceSyncOnRefresh: true,
  });
  return (
    <UsageMachineProvider value={source}>
      <DashboardToday />
      <button type="button" onClick={() => void source.refresh?.()}>
        Sync remote usage
      </button>
    </UsageMachineProvider>
  );
}

describe("SSH hosts in the main window", () => {
  beforeEach(() => {
    vi.mocked(invoke).mockReset();
  });

  it("shares the all-machine total between the sidebar and dashboard, including after manual sync", async () => {
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      switch (command) {
        case "ssh_hosts":
          return hosts;
        case "ssh_usage_snapshot":
          return {
            id: (args as { forceScan?: boolean }).forceScan
              ? "refreshed"
              : "initial",
            failures: [],
          };
        case "get_usage_summary_by_app":
          return [
            {
              appType: "codex",
              summary: {
                totalCost:
                  (args as { snapshotId?: string }).snapshotId === "refreshed"
                    ? "300.00"
                    : "293.32",
                totalRequests: 652,
                totalInputTokens: 100,
                totalOutputTokens: 100,
                totalCacheCreationTokens: 0,
                totalCacheReadTokens: 0,
                successRate: 100,
              },
            },
          ];
        default:
          return null;
      }
    });

    renderPage(
      <>
        <SidebarToday />
        <DashboardSnapshot />
      </>,
    );
    await waitFor(() => {
      expect(screen.getByLabelText("Sidebar today")).toHaveTextContent(
        "293.32",
      );
      expect(screen.getByLabelText("Dashboard today")).toHaveTextContent(
        "293.32",
      );
    });
    expect(
      vi
        .mocked(invoke)
        .mock.calls.filter(([command]) => command === "ssh_usage_snapshot"),
    ).toEqual([
      [
        "ssh_usage_snapshot",
        { hostIds: ["server-a", "server-b"], includeLocal: true },
      ],
    ]);
    expect(
      vi
        .mocked(invoke)
        .mock.calls.filter(
          ([command]) => command === "get_usage_summary_by_app",
        ),
    ).toEqual([
      [
        "get_usage_summary_by_app",
        expect.objectContaining({ snapshotId: "initial" }),
      ],
    ]);

    await userEvent.click(
      screen.getByRole("button", { name: "Sync remote usage" }),
    );
    await waitFor(() => {
      expect(screen.getByLabelText("Sidebar today")).toHaveTextContent(
        "300.00",
      );
      expect(screen.getByLabelText("Dashboard today")).toHaveTextContent(
        "300.00",
      );
    });
    expect(invoke).toHaveBeenCalledWith("ssh_usage_snapshot", {
      hostIds: ["server-a", "server-b"],
      includeLocal: true,
      forceScan: true,
    });
  });

  it("opens the original provider renderer in the selected machine frame", async () => {
    vi.mocked(invoke).mockResolvedValue({ data: { protocol: 1 } });
    const navigate = vi.fn();
    renderPage(
      <ProviderMachineFrame
        host={hosts[1]}
        app="codex"
        onNavigate={navigate}
      />,
    );
    const frame = (await screen.findByTitle("Machine B")) as HTMLIFrameElement;
    expect(frame).toHaveAttribute(
      "src",
      "./provider-machine.html?host=server-b&app=codex&name=Machine+B",
    );
    expect(invoke).toHaveBeenCalledWith("ssh_provider_command", {
      hostId: "server-b",
      command: "hello",
      args: {},
    });
    const data = {
      type: "cc-switch:provider-machine-navigation",
      page: "usage",
    };
    window.dispatchEvent(new MessageEvent("message", { data, source: window }));
    expect(navigate).not.toHaveBeenCalled();
    window.dispatchEvent(
      new MessageEvent("message", { data, source: frame.contentWindow }),
    );
    expect(navigate).toHaveBeenCalledWith("usage");
  });

  it("previews a local config and only writes the selected host after confirmation", async () => {
    const path = "/remote/.codex/config.toml";
    vi.mocked(invoke).mockImplementation(async (command) => {
      switch (command) {
        case "ssh_hosts":
          return hosts;
        case "ssh_providers":
          return [];
        case "ssh_read_config_file":
          return {
            path,
            contents: 'model = "remote"',
            revision: "saved-revision",
            exists: true,
          };
        case "ssh_read_local_config_file":
          return {
            path: "/local/.codex/config.toml",
            contents: 'model = "local"',
          };
        case "ssh_write_config_file":
          return { backupPath: null };
        default:
          throw new Error(command);
      }
    });
    const user = userEvent.setup();
    function ConfigFixture() {
      const [selection, setSelection] = useState<RemoteConfigSelection>();
      return (
        <>
          <RemoteConfigButton
            app="codex"
            host={hosts[1]}
            onSelect={(host, file) =>
              setSelection({ host, app: "codex", file })
            }
          />
          {selection && (
            <RemoteConfigEditor
              selection={selection}
              onClose={() => setSelection(undefined)}
            />
          )}
        </>
      );
    }
    renderPage(<ConfigFixture />);
    await user.click(
      screen.getByRole("button", { name: "Configuration files" }),
    );

    const editor = await screen.findByRole("textbox", { name: path });
    expect(editor).toHaveValue('model = "remote"');
    expect(invoke).toHaveBeenCalledWith("ssh_read_config_file", {
      hostId: "server-b",
      key: "codex",
    });
    await user.click(
      screen.getByRole("button", { name: "Load file from this computer" }),
    );
    await waitFor(() => expect(editor).toHaveValue('model = "local"'));
    expect(invoke).not.toHaveBeenCalledWith(
      "ssh_write_config_file",
      expect.anything(),
    );

    await user.click(screen.getByRole("button", { name: "Save to server" }));
    expect(invoke).not.toHaveBeenCalledWith(
      "ssh_write_config_file",
      expect.anything(),
    );
    const confirmation = screen.getByRole("dialog");
    expect(confirmation).toHaveTextContent("Machine B");
    expect(confirmation).toHaveTextContent(path);
    await user.click(
      within(confirmation).getByRole("button", { name: "common.save" }),
    );
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("ssh_write_config_file", {
        hostId: "server-b",
        key: "codex",
        draft: { contents: 'model = "local"', revision: "saved-revision" },
      }),
    );
  });

  it("expands the saved host list and accepts an OpenSSH alias", async () => {
    let saved = hosts;
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "ssh_hosts") return saved;
      if (command === "ssh_aliases") return ["new-alias"];
      if (command === "ssh_save_host") {
        const host = {
          ...((args as Record<string, unknown> | undefined)?.input as SshHost),
          id: "new-host",
          name: "new-alias",
        };
        saved = [...saved, host];
        return host;
      }
      if (command === "ssh_connect")
        return { cliVersion: "5.11.4", arch: "x86_64" };
      throw new Error(command);
    });
    const user = userEvent.setup();
    renderPage(<SshHosts collapsed={false} onExpand={() => {}} />);
    await user.click(screen.getByRole("button", { name: "ssh.title" }));
    expect(await screen.findByText("Machine A")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "ssh.addHost" }));
    await user.click(
      await screen.findByRole("combobox", { name: "ssh.fromConfig" }),
    );
    await user.click(await screen.findByRole("option", { name: "new-alias" }));
    await user.click(screen.getByRole("button", { name: "common.save" }));
    expect(
      await screen.findByRole("button", { name: "new-alias" }),
    ).toBeInTheDocument();
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("ssh_connect", {
        hostId: "new-host",
      }),
    );
    expect(invoke).not.toHaveBeenCalledWith("ssh_open", expect.anything());
  });
});
