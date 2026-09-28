// Fixture: a vendored library's declarations, in the shape of an @types
// package — an ambient module with overloads and a merged namespace, global
// additions, and a declaration of a function implemented in widget.ts.

declare module "widget-kit" {
  /** Opens a widget store. */
  export function openStore(path: string): Store;
  export function openStore(path: string, size: number): Store;
  export namespace openStore {
    function sync(path: string): Store;
  }

  export class Store {
    get(key: string): string;
  }
}

declare global {
  var WIDGET_DEBUG: boolean;
  function trackWidget(event: string): void;
}

declare function buildWidget(): Widget;
