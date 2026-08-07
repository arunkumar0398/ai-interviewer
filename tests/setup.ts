import { vi } from "vitest";
import "@testing-library/jest-dom/vitest";

// Mock window.__TAURI__ for all tests
Object.defineProperty(window, "__TAURI__", {
  value: {
    core: {
      invoke: vi.fn(),
      transformCallback: vi.fn(),
    },
    event: {
      listen: vi.fn(),
      emit: vi.fn(),
    },
    window: {
      getCurrentWindow: vi.fn(() => ({
        listen: vi.fn(),
        close: vi.fn(),
      })),
    },
    path: {
      appDataDir: vi.fn(),
      join: vi.fn(),
    },
  },
  writable: true,
});

// Mock window.__TAURI_INTERNALS__
Object.defineProperty(window, "__TAURI_INTERNALS__", {
  value: {
    invoke: vi.fn(),
    transformCallback: vi.fn(),
  },
  writable: true,
});

// Store to capture listen callbacks for tests that need to emit events
export const mockListenCallbacks = new Map<string, (...args: unknown[]) => void>();
export const mockListenUnlisten = vi.fn(() => Promise.resolve());

// Mock @tauri-apps/api/event module — captures callback so tests can emit events
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((event: string, cb: (...args: unknown[]) => void) => {
    mockListenCallbacks.set(event, cb);
    return Promise.resolve(mockListenUnlisten);
  }),
}));
