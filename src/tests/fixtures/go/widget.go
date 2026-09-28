// Fixture: a small, domain-neutral Go file exercising the kinds the plugin
// extracts (struct, interface→trait, named type, method with receiver, free
// func, const).

package widget

const MaxRetries = 3

const (
    ColorRed = iota
    ColorBlue
)

var defaultSize = 1

type Widget struct {
	Size int
}

type RenderFunc func(w *Widget) string

type Renderer interface {
	Render() string
}

func (w *Widget) Resize(n int) {
	w.Size = n
}

func BuildWidget() *Widget {
	return &Widget{}
}

func MaxRetriesFor(w *Widget) int {
    const limit = MaxRetries
    return limit
}

func RenderFuncFor(w *Widget) RenderFunc {
	return nil
}
