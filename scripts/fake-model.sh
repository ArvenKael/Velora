#!/bin/sh
# A stand-in for a language model.
#
# Velora needs a model to judge events, to think, and to hold a conversation, and
# a real one needs a credential. This answers all three from the shape of the
# prompt it is given, so the whole loop can be run and watched without one:
#
#   VELORA_LLM_PROVIDER=command \
#   VELORA_LLM_MODEL=fake \
#   VELORA_LLM_COMMAND=scripts/fake-model.sh \
#   cargo run -p velora-bin
#
# It is also the shape a subscription takes: a program, with the prompt on
# standard input and the answer on standard output.
#
# Two things about it are worth reading as documentation of how the system works.
#
# Procedure names are the *offered* names, which flatten the dots: a provider
# only accepts `^[a-zA-Z0-9_-]+$`, so `local.shell.run` is offered, and asked for,
# as `local_shell_run`.
#
# And answering the operator is a procedure like any other. Nothing this script
# writes in prose reaches anyone; to be heard it has to call the channel,
# `operator_say`. That is the point — whether to speak is the model's decision,
# made the same way it decides to run a command.
#
# The branches are ordered by how specific they are, and the order matters. A
# turn's prompt holds the whole assembled context, including the text of earlier
# turns' tasks, so matching a phrase anywhere in the prompt would classify a
# thought the system started itself as a conversation. The task is read from the
# task section, and a tool result is checked before it.

prompt=$(cat)

# The task, as the context assembler wrote it: the line after the heading.
task=$(printf '%s' "$prompt" | sed -n '/^## The task$/{n;p;}' | head -1)

# The last thing the operator said, from the transcript the context assembler
# built. This is the only reason the stub can answer a follow-up: it reads the
# same dialogue the model would.
said=$(printf '%s' "$prompt" | sed -n 's/^operator: //p' | tail -1 | tr -d '"\\')

# What the last command printed, for answering from the result rather than
# around it.
output=$(printf '%s' "$prompt" | awk '/^Tool result:$/{getline; print; exit}' | tr -d '"\\' | tr '\n' ' ')

# Whether this turn is a conversation. Every conversation task opens with the
# same words, so this is a prefix test rather than a search.
conversation=no
case "$task" in
  "The operator has said something"*) conversation=yes ;;
esac

say() {
  printf '%s' '{"thought":"I will answer them.","steps":[{"procedure":"operator_say","arguments":{"text":"'"$1"'"}}]}'
}

case "$prompt" in
  *"New event:"*)
    # The judge, asking what an event shows to be true. A real model reads the
    # event and records what it states; this one looks for the word it knows, so
    # that "I have a new smart lamp" becomes a belief about a lamp.
    case "$prompt" in
      *"operator.said"*)
        content=$(printf '%s' "$prompt" | sed -n 's/^content: //p' | tail -1)
        case "$content" in
          *lamp*)
            printf '%s' '{"decisions":[{"action":"assert","key":"operator.has_lamp","value":"a new smart lamp, in the bedroom, changed by an app","confidence":8000,"rationale":"the operator said so, which is the only evidence there can be for it"}]}'
            ;;
          *)
            printf '%s' '{"decisions":[]}'
            ;;
        esac
        ;;
      *)
        printf '%s' '{"decisions":[{"action":"assert","key":"operator.interest","value":"weather","confidence":6500,"rationale":"the note says the operator keeps asking about the weather"}]}'
        ;;
    esac
    ;;

  *"Tool result:"*|*"refused:"*)
    case "$prompt" in
      *"calls operator_say"*)
        # It has already said something to the operator. Concluding here is what
        # stops a model answering its own answer until the step limit: to speak
        # again it would have to have something new to say, and this one does not.
        printf '%s' '{"answer":"said."}'
        ;;
      *)
        # Some other step came back, so there is something to conclude from — and
        # the conclusion is drawn from what actually came back, not from what was
        # hoped for. If the operator is waiting, this is what they are told.
        if [ "$conversation" = "yes" ]; then
          say "I did that. It reported: $output"
        else
          printf '%s' '{"answer":"I looked, and reported what I found."}'
        fi
        ;;
    esac
    ;;

  *)
    if [ "$conversation" = "yes" ]; then
      case "$said" in
        *"who are you"*|*"what are you"*)
          say "I am Velora. I run on this machine, I keep what I learn as beliefs backed by events, and I can run things here. What do you need?"
          ;;
        *write*|*save*|*"make a file"*)
          # Something to *do*, on the real filesystem, before answering. The path
          # is under target/, which is ignored, so this proves the reach without
          # leaving anything in the repository.
          printf '%s' '{"thought":"I will write that where it can be read back.","steps":[{"procedure":"local_fs_write","arguments":{"path":"target/velora/proof.txt","contents":"Velora wrote this file through local.fs.write, in a turn that started with someone typing into a browser."}}]}'
          ;;
        *files*|*file*|*folder*|*director*|*disk*|*"in here"*)
          # Something to look at, so take a step first and answer from the result.
          printf '%s' '{"thought":"I should look before I answer.","steps":[{"procedure":"local_shell_run","arguments":{"command":"ls -1 | head -6"}}]}'
          ;;
        *)
          say "You said: $said"
          ;;
      esac
    else
      # A turn the system started on its own, from something it came to believe.
      printf '%s' '{"thought":"I will look at what is here before deciding.","steps":[{"procedure":"local_shell_run","arguments":{"command":"echo velora looked around"}}]}'
    fi
    ;;
esac
