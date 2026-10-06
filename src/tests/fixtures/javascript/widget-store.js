// Fixture: a domain-neutral CommonJS module, which defines its API by
// assigning functions to members at the top level: a constructor's prototype,
// an exported object, `exports` itself.

"use strict";

var proto = (module.exports = {});

function WidgetStore(options) {
  this.options = options;
}

WidgetStore.prototype.render = function render(name) {
  return this.options[name];
};

proto.listen = function listen(port) {
  return port;
};

exports.createWidgetStore = function (options) {
  return new WidgetStore(options);
};

exports.WidgetStore = WidgetStore;

function setup() {
  proto.reset = () => {};
}
