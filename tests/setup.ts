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
