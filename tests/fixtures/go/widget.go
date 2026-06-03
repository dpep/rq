// Fixture: a small, domain-neutral Go file exercising the kinds the plugin
// extracts (struct, interface→trait, method with receiver, free func).

package widget

type Widget struct {
	Size int
}

type Renderer interface {
	Render() string
}

func (w *Widget) Resize(n int) {
	w.Size = n
}

func BuildWidget() *Widget {
	return &Widget{}
}
