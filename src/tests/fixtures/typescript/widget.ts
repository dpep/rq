// Fixture: a small, domain-neutral TypeScript file exercising the kinds the
// plugin extracts (class, interface→trait, type, enum and its members, method,
// function, an arrow assigned to a const, constants, and the properties of
// classes, interfaces and object types, static or not), and an exported `let`.

export const MAX_RETRIES = 3;

export interface Renderer {
  render(): string;
}

export interface WidgetOptions {
  color: WidgetColor;
  defaultWidget?: Widget;
}

export const WIDGET_DEFAULTS = { label: "widget" };

export let activeWidget: Widget | undefined;

export type WidgetSize = {
  width: number;
  height: number;
};

export enum WidgetColor {
  Red,
  Green,
}

export class Widget implements Renderer {
  static readonly DEFAULT_WIDTH = 1;
  private owner?: string;

  static instances = 0;

  constructor(private size: WidgetSize) {}

  static create(): Widget {
    return new Widget({ width: 1, height: 1 });
  }

  static get registry(): Widget[] {
    return [];
  }

  render(): string {
    return `${this.size.width}x${this.size.height}`;
  }

  resize(width: number): void {
    this.size = { ...this.size, width };
  }
}

/** Resizes every widget to one size. */
export function resizeAll(widgets: Widget[], width: number): void;
export function resizeAll(widgets: Widget[], size: WidgetSize): void;
export function resizeAll(widgets: Widget[], size: number | WidgetSize): void {
  for (const w of widgets) {
    w.resize(typeof size === "number" ? size : size.width);
  }
}

export function buildWidget(): Widget {
  return new Widget({ width: 1, height: 1 });
}

export const defaultWidget = () => buildWidget();

export function maxRetriesFor(widget: Widget): number {
  return MAX_RETRIES;
}
