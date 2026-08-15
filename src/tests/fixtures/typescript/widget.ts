// Fixture: a small, domain-neutral TypeScript file exercising the kinds the
// plugin extracts (class, interface→trait, type→struct, enum, method, function,
// and an arrow assigned to a const).

export interface Renderer {
  render(): string;
}

export type WidgetSize = {
  width: number;
  height: number;
};

export enum WidgetColor {
  Red,
  Green,
}

export class Widget implements Renderer {
  constructor(private size: WidgetSize) {}

  render(): string {
    return `${this.size.width}x${this.size.height}`;
  }

  resize(width: number): void {
    this.size = { ...this.size, width };
  }
}

export function buildWidget(): Widget {
  return new Widget({ width: 1, height: 1 });
}

export const defaultWidget = () => buildWidget();
