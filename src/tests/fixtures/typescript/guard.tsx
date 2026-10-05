// Fixture for the extraction guard only: TypeScript with JSX, which the .ts
// fixtures can't hold.

interface GadgetProps {
  label: string;
}

export const GadgetBadge = ({ label }: GadgetProps) => <span>{label}</span>;

export function GadgetList(props: { items: string[] }) {
  return <ul>{props.items.map((item) => <li>{item}</li>)}</ul>;
}
