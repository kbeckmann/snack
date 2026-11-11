//! Keep Iced's shrink-sized editor within its padded height limit.
use crate::Message;
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, overlay, renderer, widget::{Operation, Tree}};
use iced::{Element, Event, Length, Rectangle, Size, Vector, mouse};

pub(crate) fn bounded_editor(content: Element<'_, Message>, max_height: f32) -> Element<'_, Message> {
    Element::new(BoundedEditor { content, max_height })
}

struct BoundedEditor<'a> {
    content: Element<'a, Message>,
    max_height: f32,
}

impl Widget<Message, iced::Theme, iced::Renderer> for BoundedEditor<'_> {
    fn children(&self) -> Vec<Tree> { vec![Tree::new(&self.content)] }
    fn diff(&self, tree: &mut Tree) { tree.diff_children(std::slice::from_ref(&self.content)); }
    fn size(&self) -> Size<Length> { self.content.as_widget().size() }
    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        let limits = limits.max_height(self.max_height);
        let node = self.content.as_widget_mut().layout(&mut tree.children[0], renderer, &limits);
        // TextEditor is a leaf. Iced 0.14 prepares its text viewport using the
        // padded limits, but Shrink adds padding after resolving the node's
        // height limit. All editor operations must see the bounded viewport,
        // including drawing, caret scrolling, hit testing and IME placement.
        debug_assert!(node.children().is_empty());
        layout::Node::new(Size::new(node.size().width, node.size().height.min(limits.max().height)))
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &iced::Renderer, operation: &mut dyn Operation) {
        self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor,
        renderer: &iced::Renderer, clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, viewport: &Rectangle) {
        self.content.as_widget_mut().update(&mut tree.children[0], event, layout, cursor, renderer, clipboard, shell, viewport);
    }
    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor,
        viewport: &Rectangle, renderer: &iced::Renderer) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }
    fn draw(&self, tree: &Tree, renderer: &mut iced::Renderer, theme: &iced::Theme,
        style: &renderer::Style, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }
    fn overlay<'b>(&'b mut self, tree: &'b mut Tree, layout: Layout<'b>, renderer: &iced::Renderer,
        viewport: &Rectangle, translation: Vector) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}
