// Fixture for the extraction guard only: the forms account.jsx leaves out —
// class fields, public, private and static.

export class Gadget {
  size = 1;
  #secret = 2;
  static count = 0;

  get label() {
    return `${this.size}`;
  }
}
