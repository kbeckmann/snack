//! Observes transcript drags even when the embedded editor captures the event.
use crate::Message;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer, widget::Operation, widget::Tree,
};
use iced::{Element, Event, Length, Rectangle, Size, Vector, mouse};

pub struct SelectionArea<'a> {
    content: Element<'a, Message>,
    id: i64,
    dragging: bool,
    selected: bool,
}
pub fn selection_area<'a>(
    content: Element<'a, Message>,
    id: i64,
    dragging: bool,
    selected: bool,
) -> Element<'a, Message> {
    Element::new(SelectionArea {
        content,
        id,
        dragging,
        selected,
    })
}
impl Widget<Message, iced::Theme, iced::Renderer> for SelectionArea<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // Observe before delegation: another editor may already have captured
        // this movement while clamping its own drag to its bounds.
        if self.dragging && matches!(event, Event::Mouse(mouse::Event::CursorMoved { .. })) {
            if let Some(position) = cursor.position() {
                let bounds = layout.bounds();
                if position.y >= bounds.y
                    && position.y < bounds.y + bounds.height
                    && viewport.contains(position)
                {
                    shell.publish(Message::BodyDrag {
                        id: self.id,
                        point: iced::Point::new(
                            (position.x - bounds.x).clamp(0.0, bounds.width),
                            position.y - bounds.y,
                        ),
                    });
                }
            }
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        renderer_style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        // Iced only paints a selection for focused editors. Paint using a
        // temporary focused state so all selected rows show their ranges,
        // without granting keyboard focus to multiple editors.
        let mut painted_tree;
        let body_tree = if self.selected {
            use iced::advanced::widget::operation::Focusable;
            painted_tree = Tree::new(&self.content);
            painted_tree.state.downcast_mut::<iced::widget::text_editor::State<iced::advanced::text::highlighter::PlainText>>().focus();
            &painted_tree
        } else {
            &tree.children[0]
        };
        self.content.as_widget().draw(
            body_tree,
            renderer,
            theme,
            renderer_style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}
